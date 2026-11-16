//! Fetches a page and runs the WCAG rule engine against it.

use crate::checks::{self, Level, Outcome, RuleResult, Severity, Version};
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
