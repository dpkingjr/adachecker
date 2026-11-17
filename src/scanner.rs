//! Fetches a page and runs the WCAG rule engine against it.

use crate::checks::{self, Level, Locator, Outcome, RuleResult, Severity, Version};
use crate::links::{self, CheckedLink, Kind};
use std::collections::HashSet;
use crate::source::{self, SourceDoc};
use std::sync::Arc;
use scraper::{Html, Selector};
use std::time::{Duration, Instant};
use url::Url;

#[derive(Clone, Debug)]
pub struct Report {
    pub final_url: String,
    pub status: Option<u16>,
    pub page_title: Option<String>,
    pub version: Version,
    pub level: Level,
    pub bytes: usize,
    pub elapsed: Duration,
    pub results: Vec<RuleResult>,
    /// The raw HTML that was analyzed, for the source view.
    pub source: Arc<SourceDoc>,
    /// Every link on the page, in document order (for the optional link check).
    pub links: Vec<PageLink>,
}

/// A link found on the scanned page.
#[derive(Clone, Debug)]
pub struct PageLink {
    /// The address resolved against the page (and any `<base href>`).
    pub url: String,
    /// The link's text (or image alt / aria-label), for display.
    pub text: String,
    /// Where the `<a>`/`<area>` element is, for highlighting.
    pub locator: Locator,
    /// For links to this same page (`#id`): whether the target exists. Checked during the
    /// scan; other links are checked over the network.
    pub local_anchor: Option<bool>,
}

/// How a page link's check came out, grouped for counting and colouring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkBucket {
    Pending,
    Ok,
    Redirected,
    /// Couldn't be confirmed either way (401/403/429, or an anchor that couldn't be checked).
    Review,
    Broken,
}

/// The bucket and a short description for a page link, given its network check (if any).
pub fn link_status(link: &PageLink, checked: Option<&CheckedLink>) -> (LinkBucket, String) {
    let fragment = || link.url.split_once('#').map(|(_, f)| f.to_string()).unwrap_or_default();
    if let Some(found) = link.local_anchor {
        return if found {
            (LinkBucket::Ok, "In-page link; target exists".into())
        } else {
            (LinkBucket::Broken, format!("In-page link: #{} is not on this page", fragment()))
        };
    }
    let Some(c) = checked else { return (LinkBucket::Pending, "Checking…".into()) };
    let r = &c.result;
    match (r.kind, &c.anchor) {
        (Kind::Ok | Kind::Redirected, Some(Ok(false))) => (LinkBucket::Broken, format!("{} but #{} is not on that page", r.status_text(), fragment())),
        (Kind::Ok | Kind::Redirected, Some(Err(e))) => (LinkBucket::Review, format!("{}; anchor not checked: {e}", r.status_text())),
        (Kind::Ok, _) => (LinkBucket::Ok, r.status_text()),
        (Kind::Redirected, _) => (LinkBucket::Redirected, format!("{} → {}", r.chain_text(), r.final_url)),
        (Kind::Restricted, _) => (LinkBucket::Review, format!("{} — may work in a browser", r.status_text())),
        (Kind::Broken, _) => (LinkBucket::Broken, r.status_text()),
        (Kind::Error, _) if r.timed_out() => (LinkBucket::Review, "Timed out — the site may be slow or only reachable on another network".into()),
        (Kind::Error, _) => (LinkBucket::Broken, r.reason.clone()),
    }
}

/// Collects the page's links. Skips `mailto:`, `tel:`, `javascript:` and similar, and
/// anything that isn't http(s) once resolved.
pub fn extract_links(doc: &Html, page_url: &str) -> Vec<PageLink> {
    let Ok(page) = Url::parse(page_url) else { return Vec::new() };
    let sel = |s: &str| Selector::parse(s).expect("valid selector");
    let base = doc
        .select(&sel("base[href]"))
        .next()
        .and_then(|b| b.value().attr("href"))
        .and_then(|h| page.join(h.trim()).ok())
        .unwrap_or_else(|| page.clone());
    let mut targets: HashSet<String> = doc.select(&sel("[id]")).filter_map(|e| e.value().attr("id")).map(str::to_string).collect();
    targets.extend(doc.select(&sel("a[name]")).filter_map(|e| e.value().attr("name")).map(str::to_string));
    let without_fragment = |u: &Url| {
        let mut u = u.clone();
        u.set_fragment(None);
        u
    };
    let page_doc = without_fragment(&page);

    let mut out = Vec::new();
    for el in doc.select(&sel("a[href], area[href]")) {
        let href = el.value().attr("href").unwrap_or("").trim();
        let lower = href.to_ascii_lowercase();
        if href.is_empty() || ["mailto:", "tel:", "javascript:", "data:", "sms:", "about:"].iter().any(|p| lower.starts_with(p)) {
            continue;
        }
        let Ok(url) = base.join(href) else { continue };
        if !matches!(url.scheme(), "http" | "https" | "file") {
            continue;
        }
        let text = el
            .value()
            .attr("aria-label")
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .or_else(|| Some(checks::content_text(el)).filter(|t| !t.is_empty()))
            .or_else(|| el.value().attr("title").map(|t| t.trim().to_string()))
            .unwrap_or_default();
        // A link to this same page is checked here, against the page's own ids.
        let local_anchor = match url.fragment() {
            Some(f) if without_fragment(&url) == page_doc => {
                let f = links::percent_decode(f);
                // Empty, "#top" and app routes ("#!/…") aren't element anchors: fine as-is.
                Some(!links::is_element_fragment(&f) || f.eq_ignore_ascii_case("top") || targets.contains(&f))
            }
            _ if url.scheme() == "file" => continue,
            _ => None,
        };
        out.push(PageLink { url: url.to_string(), text, locator: checks::locator(el), local_anchor });
    }
    out
}

impl Report {
    pub fn total(&self, sev: Severity) -> usize {
        self.results.iter().map(|r| r.count(sev)).sum()
    }

    pub fn outcomes(&self, o: Outcome) -> usize {
        self.results.iter().filter(|r| r.outcome() == o).count()
    }

    /// Plain-text version of the report for copying to the clipboard.
    pub fn to_text(&self) -> String {
        let mut s = format!(
            "WCAG {} Level {} scan\nURL: {}\nTitle: {}\nCriteria failed: {}  Needs review: {}  Passed: {}\nErrors: {}  Warnings: {}\n\n",
            self.version,
            self.level,
            self.final_url,
            self.page_title.as_deref().unwrap_or("(none)"),
            self.outcomes(Outcome::Fail),
            self.outcomes(Outcome::Review),
            self.outcomes(Outcome::Pass),
            self.total(Severity::Error),
            self.total(Severity::Warning),
        );
        for r in &self.results {
            let tag = match r.outcome() {
                Outcome::Pass => "PASS",
                Outcome::Review => "REVIEW",
                Outcome::Fail => "FAIL",
            };
            let new_in = if r.since > Version::V2_0 { format!(", new in {}", r.since) } else { String::new() };
            s.push_str(&format!("[{tag}] {} (Level {}{new_in}) {}\n", r.criterion, r.level, r.title));
            for f in &r.findings {
                let sev = if f.severity == Severity::Error { "error" } else { "warning" };
                s.push_str(&format!("    - {sev}: {}\n", f.message));
                if let Some(sn) = &f.snippet {
                    s.push_str(&format!("      {sn}\n"));
                }
            }
        }
        s
    }
}

/// Turns user input into a URL, defaulting to https:// when no scheme is given.
/// Local paths are accepted and converted to file:// URLs.
pub fn normalize_url(input: &str) -> Result<Url, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("Please enter a URL.".into());
    }
    if std::path::Path::new(input).is_absolute() && std::path::Path::new(input).exists() {
        return Url::from_file_path(input).map_err(|_| "Invalid file path.".into());
    }
    let with_scheme = if input.contains("://") { input.to_string() } else { format!("https://{input}") };
    let url = Url::parse(&with_scheme).map_err(|e| format!("Invalid URL: {e}"))?;
    match url.scheme() {
        "http" | "https" | "file" => Ok(url),
        other => Err(format!("Unsupported URL scheme \"{other}\".")),
    }
}

fn fetch(url: &Url) -> Result<(String, Option<u16>, String), String> {
    if url.scheme() == "file" {
        let path = url.to_file_path().map_err(|_| "Invalid file URL.".to_string())?;
        let body = std::fs::read_to_string(&path).map_err(|e| format!("Could not read {}: {e}", path.display()))?;
        return Ok((url.to_string(), None, body));
    }
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("ADAChecker/", env!("CARGO_PKG_VERSION"), " (WCAG accessibility scanner)"))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("Could not create HTTP client: {e}"))?;
    let resp = client
        .get(url.clone())
        .header("Accept", "text/html,application/xhtml+xml;q=0.9,*/*;q=0.5")
        .send()
        .map_err(|e| format!("Request failed: {e}"))?;
    let status = resp.status();
    let final_url = resp.url().to_string();
    if let Some(ct) = resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()) {
        if !ct.contains("html") && !ct.contains("xml") {
            return Err(format!("The URL returned \"{ct}\", not an HTML page."));
        }
    }
    let body = resp.text().map_err(|e| format!("Could not read response body: {e}"))?;
    if !status.is_success() {
        return Err(format!("Server responded with HTTP {status}."));
    }
    Ok((final_url, Some(status.as_u16()), body))
}

pub fn scan(input: &str, version: Version, level: Level) -> Result<Report, String> {
    let start = Instant::now();
    let url = normalize_url(input)?;
    let (final_url, status, body) = fetch(&url)?;
    let doc = Html::parse_document(&body);
    let page_title = doc
        .select(&Selector::parse("title").expect("valid selector"))
        .next()
        .map(|t| t.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|t| !t.is_empty());
    let mut results = checks::run(&doc, version, level);
    let bytes = body.len();
    let source_doc = SourceDoc::new(body);
    let spans = source::map_elements(&doc, &source_doc.tags);
    for f in results.iter_mut().flat_map(|r| r.findings.iter_mut()) {
        if let Some(loc) = f.locator.as_mut() {
            loc.source = spans.get(&loc.node).cloned();
        }
    }
    let mut page_links = extract_links(&doc, &final_url);
    for l in &mut page_links {
        l.locator.source = spans.get(&l.locator.node).cloned();
    }
    Ok(Report {
        final_url,
        status,
        page_title,
        version,
        level,
        bytes,
        elapsed: start.elapsed(),
        results,
        source: Arc::new(source_doc),
        links: page_links,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "requires network"]
    fn live_scan() {
        let r = super::scan(&std::env::var("SCAN_URL").unwrap_or_else(|_| "example.com".into()), crate::checks::Version::V2_1, crate::checks::Level::AA).expect("scan ok");
        println!("{}", r.to_text());
        let (mut located, mut total) = (0, 0);
        for f in r.results.iter().flat_map(|x| x.findings.iter()) {
            if let Some(l) = &f.locator {
                total += 1;
                if let Some(s) = &l.source {
                    located += 1;
                    let tag = &r.source.text[s.clone()];
                    assert!(tag.to_ascii_lowercase().starts_with(&format!("<{}", l.tag.to_ascii_lowercase())), "span {tag:?} is not a <{}>", l.tag);
                }
            }
        }
        println!("source spans located for {located}/{total} element findings");
    }
}

#[cfg(test)]
mod source_mapping_tests {
    use crate::checks::{Level, Version};

    #[test]
    fn findings_point_at_their_source_tags() {
        let dir = std::env::temp_dir().join(format!("adachecker-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("page.html");
        let html = "<!doctype html>\n<html>\n<body>\n<table><tr><td>x</td></tr></table>\n<p>One</p>\n  <img src=\"a.png\">\n<a href=\"/x\"></a>\n</body></html>";
        std::fs::write(&path, html).unwrap();
        let r = super::scan(path.to_str().unwrap(), Version::V2_1, Level::A).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        let span = |title: &str| {
            let rule = r.results.iter().find(|x| x.title.contains(title)).unwrap();
            let s = rule.findings[0].locator.as_ref().unwrap().source.clone().unwrap();
            (r.source.text[s.clone()].to_string(), r.source.line_of(s.start) + 1)
        };
        assert_eq!(span("Non-text"), ("<img src=\"a.png\">".to_string(), 6));
        assert_eq!(span("Link Purpose"), ("<a href=\"/x\">".to_string(), 7));
    }
}

#[cfg(test)]
mod link_extraction_tests {
    use super::*;

    #[test]
    fn extracts_and_resolves_page_links() {
        let html = r##"<html><head><base href="/docs/"></head><body>
            <h2 id="intro">Intro</h2>
            <a href="guide.html">Guide</a>
            <a href="https://other.example/x#part">Other</a>
            <a href="#intro">Jump</a>
            <a href="#missing">Broken jump</a>
            <a href="/page.html#intro" aria-label="Self">x</a>
            <a href="mailto:a@b.c">Mail</a><a href="javascript:void(0)">JS</a><a href="">Empty</a>
            <a href="#"><img src="i.png" alt="Top"></a>
        </body></html>"##;
        let doc = Html::parse_document(html);
        let links = extract_links(&doc, "https://site.example/page.html");
        let got: Vec<(&str, &str, Option<bool>)> = links.iter().map(|l| (l.url.as_str(), l.text.as_str(), l.local_anchor)).collect();
        assert_eq!(
            got,
            [
                ("https://site.example/docs/guide.html", "Guide", None),
                ("https://other.example/x#part", "Other", None),
                ("https://site.example/docs/#intro", "Jump", None),
                ("https://site.example/docs/#missing", "Broken jump", None),
                ("https://site.example/page.html#intro", "Self", Some(true)),
                ("https://site.example/docs/#", "Top", None),
            ],
            "<base href> applies to fragment-only links too, as in browsers"
        );
        let doc2 = Html::parse_document(r##"<a href="#intro">a</a><a href="#gone">b</a><a href="#top">c</a><a href="#!/app/route">d</a><p id="intro"></p>"##);
        let local: Vec<Option<bool>> = extract_links(&doc2, "https://s.example/p").iter().map(|l| l.local_anchor).collect();
        assert_eq!(local, [Some(true), Some(false), Some(true), Some(true)], "#!/ app routes aren't anchors");
        assert_eq!(link_status(&extract_links(&doc2, "https://s.example/p")[1], None).0, LinkBucket::Broken);
        assert_eq!(link_status(&extract_links(&doc, "https://site.example/page.html")[0], None).0, LinkBucket::Pending);
    }
}
