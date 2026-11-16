//! WCAG 2.0 / 2.1 / 2.2 rule engine.
//!
//! Every rule performs a static analysis of the delivered HTML document. Rules are
//! tagged with the WCAG success criterion, the conformance level, and the WCAG
//! versions the criterion belongs to. A scan for "WCAG 2.1 AA" runs every A and AA
//! rule that exists in WCAG 2.1 (i.e. 2.0 criteria plus those added in 2.1, and the
//! 2.1 definition of 4.1.1 Parsing, which was removed in 2.2).

use scraper::{ElementRef, Html, Node, Selector};
use std::collections::HashMap;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    A,
    AA,
    AAA,
}

impl Level {
    pub const ALL: [Level; 3] = [Level::A, Level::AA, Level::AAA];
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Level::A => write!(f, "A"),
            Level::AA => write!(f, "AA"),
            Level::AAA => write!(f, "AAA"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Version {
    V2_0,
    V2_1,
    V2_2,
}

impl Version {
    pub const ALL: [Version; 3] = [Version::V2_0, Version::V2_1, Version::V2_2];
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Version::V2_0 => write!(f, "2.0"),
            Version::V2_1 => write!(f, "2.1"),
            Version::V2_2 => write!(f, "2.2"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// Definite failure of the success criterion.
    Error,
    /// Likely problem that needs a human to confirm.
    Warning,
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub severity: Severity,
    pub message: String,
    pub snippet: Option<String>,
    /// Where the offending element lives in the DOM, for highlighting it in the preview.
    /// `None` for page-level findings.
    pub locator: Option<Locator>,
}

/// Several independent ways to find an element again in a live DOM, which may differ
/// from the static HTML because of scripts. They are tried in order: id, CSS path, then
/// tag + distinctive attribute.
#[derive(Clone, Debug)]
pub struct Locator {
    pub tag: String,
    pub id: Option<String>,
    /// `html > body:nth-of-type(1) > div:nth-of-type(3) > img:nth-of-type(1)`
    pub path: String,
    pub attr: Option<(String, String)>,
    /// Parsed-tree node, used to look up the element's source position.
    pub node: ego_tree::NodeId,
    /// Byte range of the element's start tag in the raw HTML, filled in by the scanner.
    pub source: Option<std::ops::Range<usize>>,
}

fn locator(el: ElementRef) -> Locator {
    let mut parts = Vec::new();
    let mut cur = Some(el);
    while let Some(e) = cur {
        let name = e.value().name();
        let parent = e.parent().and_then(ElementRef::wrap);
        if parent.is_none() {
            parts.push(name.to_string());
        } else {
            let n = e.prev_siblings().filter_map(ElementRef::wrap).filter(|s| s.value().name() == name).count() + 1;
            parts.push(format!("{name}:nth-of-type({n})"));
        }
        cur = parent;
    }
    parts.reverse();
    let v = el.value();
    let id = v.attr("id").filter(|i| !i.is_empty()).map(str::to_string);
    let attr = ["src", "href", "name", "for", "action", "aria-label", "alt", "title", "data-testid"]
        .iter()
        .find_map(|a| v.attr(a).filter(|x| !x.is_empty() && x.len() < 400).map(|x| (a.to_string(), x.to_string())));
    Locator { tag: v.name().to_string(), id, path: parts.join(" > "), attr, node: el.id(), source: None }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    Review,
    Fail,
}

#[derive(Clone, Debug)]
pub struct RuleResult {
    pub criterion: &'static str,
    pub title: &'static str,
    pub level: Level,
    /// WCAG version that introduced the success criterion.
    pub since: Version,
    pub findings: Vec<Finding>,
}

impl RuleResult {
    pub fn outcome(&self) -> Outcome {
        if self.findings.iter().any(|f| f.severity == Severity::Error) {
            Outcome::Fail
        } else if self.findings.is_empty() {
            Outcome::Pass
        } else {
            Outcome::Review
        }
    }
    pub fn count(&self, sev: Severity) -> usize {
        self.findings.iter().filter(|f| f.severity == sev).count()
    }
}

// ---------------------------------------------------------------------------
// Rule definitions
// ---------------------------------------------------------------------------

struct Rule {
    criterion: &'static str,
    title: &'static str,
    level: Level,
    /// Version that introduced the criterion (shown as a "new in" badge).
    since: Version,
    /// Inclusive range of WCAG versions in which this rule is evaluated.
    first: Version,
    last: Version,
    check: fn(&Ctx) -> Vec<Finding>,
}

/// A rule for a criterion introduced in `since` and still present in WCAG 2.2.
const fn rule(criterion: &'static str, title: &'static str, level: Level, since: Version, check: fn(&Ctx) -> Vec<Finding>) -> Rule {
    Rule { criterion, title, level, since, first: since, last: Version::V2_2, check }
}

use Level::{A, AA, AAA};
use Version::{V2_0, V2_1, V2_2};

const RULES: &[Rule] = &[
    // ----- Level A -----
    rule("1.1.1", "Non-text Content: images have text alternatives", A, V2_0, check_images),
    rule("1.2.1 / 1.2.2", "Time-based Media: captions and transcripts", A, V2_0, check_media_captions),
    rule("1.3.1", "Info and Relationships: form fields have labels", A, V2_0, check_form_labels),
    rule("1.3.1", "Info and Relationships: heading structure", A, V2_0, check_heading_structure),
    rule("1.3.1", "Info and Relationships: data tables have headers", A, V2_0, check_tables),
    rule("1.4.2", "Audio Control: no unmuted autoplay", A, V2_0, check_autoplay),
    rule("2.1.1", "Keyboard: click handlers on non-focusable elements", A, V2_0, check_click_handlers),
    rule("2.1.4", "Character Key Shortcuts", A, V2_1, check_character_shortcuts),
    rule("2.2.1", "Timing Adjustable: no timed meta refresh", A, V2_0, check_meta_refresh),
    rule("2.2.2", "Pause, Stop, Hide: no blink/marquee", A, V2_0, check_blink_marquee),
    rule("2.4.1", "Bypass Blocks: landmarks or skip link", A, V2_0, check_bypass_blocks),
    rule("2.4.2", "Page Titled", A, V2_0, check_page_title),
    rule("2.4.3", "Focus Order: no positive tabindex", A, V2_0, check_tabindex),
    rule("2.4.4", "Link Purpose (In Context)", A, V2_0, check_link_purpose),
    rule("2.5.1", "Pointer Gestures: single-pointer alternatives", A, V2_1, check_pointer_gestures),
    rule("2.5.2", "Pointer Cancellation: no down-event activation", A, V2_1, check_pointer_cancellation),
    rule("2.5.3", "Label in Name", A, V2_1, check_label_in_name),
    rule("2.5.4", "Motion Actuation", A, V2_1, check_motion_actuation),
    rule("3.1.1", "Language of Page", A, V2_0, check_lang),
    rule("4.1.2", "Name, Role, Value: buttons have names", A, V2_0, check_buttons),
    rule("4.1.2", "Name, Role, Value: frames have titles", A, V2_0, check_frames),
    rule("4.1.2", "Name, Role, Value: aria-hidden on focusable", A, V2_0, check_aria_hidden_focusable),
    rule("4.1.2", "Name, Role, Value: valid ARIA roles", A, V2_0, check_aria_roles),
    // 4.1.1 Parsing is a real criterion in 2.0/2.1 and was removed (obsolete) in 2.2.
    Rule { criterion: "4.1.1", title: "Parsing: unique id attributes", level: A, since: V2_0, first: V2_0, last: V2_1, check: check_parsing_ids },
    Rule { criterion: "4.1.1 (removed in 2.2)", title: "Unique id attributes (best practice)", level: A, since: V2_0, first: V2_2, last: V2_2, check: check_duplicate_ids },
    // ----- Level AA -----
    rule("1.2.5", "Audio Description (Prerecorded)", AA, V2_0, check_audio_description),
    rule("1.3.4", "Orientation: not locked to portrait/landscape", AA, V2_1, check_orientation),
    rule("1.3.5", "Identify Input Purpose: autocomplete", AA, V2_1, check_autocomplete),
    rule("1.4.3", "Contrast (Minimum) — inline styles", AA, V2_0, check_contrast_aa),
    rule("1.4.4", "Resize Text: zoom not disabled", AA, V2_0, check_viewport_zoom),
    rule("1.4.10", "Reflow: usable at 320 CSS px width", AA, V2_1, check_reflow),
    rule("1.4.11", "Non-text Contrast: form control boundaries", AA, V2_1, check_non_text_contrast),
    rule("1.4.12", "Text Spacing: user overrides not blocked", AA, V2_1, check_text_spacing),
    rule("1.4.13", "Content on Hover or Focus", AA, V2_1, check_content_on_hover),
    rule("2.4.6", "Headings and Labels: not empty", AA, V2_0, check_empty_headings),
    rule("2.4.7", "Focus Visible: focus outline not removed", AA, V2_0, check_focus_visible),
    rule("2.4.11", "Focus Not Obscured (Minimum)", AA, V2_2, check_focus_not_obscured),
    rule("2.5.8", "Target Size (Minimum): 24×24 px", AA, V2_2, check_target_size_aa),
    rule("3.1.2", "Language of Parts: valid lang values", AA, V2_0, check_lang_parts),
    rule("3.3.8", "Accessible Authentication (Minimum)", AA, V2_2, check_accessible_authentication),
    rule("4.1.3", "Status Messages: announced via live regions", AA, V2_1, check_status_messages),
    // ----- Level AAA -----
    rule("1.3.6", "Identify Purpose: landmark regions", AAA, V2_1, check_identify_purpose),
    rule("1.4.6", "Contrast (Enhanced) — inline styles", AAA, V2_0, check_contrast_aaa),
    rule("2.3.3", "Animation from Interactions: reduced motion", AAA, V2_1, check_animation_interactions),
    rule("2.4.9", "Link Purpose (Link Only)", AAA, V2_0, check_link_only),
    rule("2.4.10", "Section Headings", AAA, V2_0, check_section_headings),
    rule("2.5.5", "Target Size (Enhanced): 44×44 px", AAA, V2_1, check_target_size_aaa),
    rule("3.2.5", "Change on Request: new windows announced", AAA, V2_0, check_new_windows),
];

/// Runs every rule that belongs to `version` and is at or below `level`.
pub fn run(doc: &Html, version: Version, level: Level) -> Vec<RuleResult> {
    let ctx = Ctx::new(doc);
    RULES
        .iter()
        .filter(|r| r.level <= level && r.first <= version && version <= r.last)
        .map(|r| RuleResult {
            criterion: r.criterion,
            title: r.title,
            level: r.level,
            since: r.since,
            findings: (r.check)(&ctx),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

struct Ctx<'a> {
    doc: &'a Html,
    ids: HashMap<&'a str, ElementRef<'a>>,
    /// All inline `<style>` CSS: lower-cased, comments and whitespace removed.
    css: String,
    /// All inline `<script>` code: lower-cased, whitespace removed.
    js: String,
}

impl<'a> Ctx<'a> {
    fn new(doc: &'a Html) -> Self {
        let mut ids = HashMap::new();
        for el in doc.select(&sel("[id]")) {
            if let Some(id) = el.value().attr("id") {
                ids.entry(id).or_insert(el);
            }
        }
        let gather = |s: &str| -> String {
            doc.select(&sel(s))
                .flat_map(|e| e.text())
                .collect::<String>()
                .to_lowercase()
        };
        let css = strip_css_comments(&gather("style"))
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let js = gather("script:not([src])")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        Ctx { doc, ids, css, js }
    }

    fn select(&self, s: &str) -> Vec<ElementRef<'a>> {
        self.doc.select(&sel(s)).collect()
    }

    /// Simplified accessible-name computation (aria-labelledby, aria-label,
    /// content incl. image alt text, then title).
    fn accessible_name(&self, el: ElementRef) -> String {
        if let Some(ids) = el.value().attr("aria-labelledby") {
            let name: Vec<String> = ids
                .split_whitespace()
                .filter_map(|id| self.ids.get(id))
                .map(|e| content_text(*e))
                .collect();
            let name = collapse_ws(&name.join(" "));
            if !name.is_empty() {
                return name;
            }
        }
        if let Some(l) = attr_nonempty(el, "aria-label") {
            return l;
        }
        let text = content_text(el);
        if !text.is_empty() {
            return text;
        }
        attr_nonempty(el, "title").unwrap_or_default()
    }
}

fn sel(s: &str) -> Selector {
    Selector::parse(s).expect("valid selector")
}

fn attr_nonempty(el: ElementRef, name: &str) -> Option<String> {
    el.value()
        .attr(name)
        .map(collapse_ws)
        .filter(|s| !s.is_empty())
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

/// Visible text content of an element, including alt text of contained images,
/// skipping subtrees hidden with aria-hidden="true".
fn content_text(el: ElementRef) -> String {
    let mut out = String::new();
    collect_text(el, &mut out);
    collapse_ws(&out)
}

fn collect_text(el: ElementRef, out: &mut String) {
    for child in el.children() {
        match child.value() {
            Node::Text(t) => {
                out.push_str(t);
                out.push(' ');
            }
            Node::Element(e) => {
                if e.attr("aria-hidden") == Some("true") {
                    continue;
                }
                if matches!(e.name(), "script" | "style" | "template" | "desc") {
                    continue;
                }
                // Per accname: a descendant's own aria-label takes precedence over its content.
                if let Some(label) = e.attr("aria-label").filter(|l| !l.trim().is_empty()) {
                    out.push_str(label);
                    out.push(' ');
                } else if e.name() == "img" || e.name() == "area" {
                    if let Some(a) = e.attr("alt") {
                        out.push_str(a);
                        out.push(' ');
                    }
                } else if let Some(child_el) = ElementRef::wrap(child) {
                    // Recursing into <svg> picks up its <title>, which names it.
                    collect_text(child_el, out);
                }
            }
            _ => {}
        }
    }
}

/// Short, readable representation of an element for the report.
fn snippet(el: ElementRef) -> String {
    let v = el.value();
    let mut s = format!("<{}", v.name());
    for (k, val) in v.attrs() {
        if matches!(k, "style" | "class") && val.len() > 40 {
            s.push_str(&format!(" {k}=\"{}\"", truncate(val, 40)));
        } else {
            s.push_str(&format!(" {k}=\"{}\"", truncate(val, 80)));
        }
    }
    s.push('>');
    let text = content_text(el);
    if !text.is_empty() {
        s.push_str(&truncate(&text, 60));
    }
    truncate(&s, 300)
}

fn err(msg: impl Into<String>, el: Option<ElementRef>) -> Finding {
    Finding { severity: Severity::Error, message: msg.into(), snippet: el.map(snippet), locator: el.map(locator) }
}

fn warn(msg: impl Into<String>, el: Option<ElementRef>) -> Finding {
    Finding { severity: Severity::Warning, message: msg.into(), snippet: el.map(snippet), locator: el.map(locator) }
}

fn has_ancestor(el: ElementRef, name: &str) -> bool {
    el.ancestors()
        .filter_map(ElementRef::wrap)
        .any(|a| a.value().name() == name)
}

fn is_hidden(el: ElementRef) -> bool {
    std::iter::once(el)
        .chain(el.ancestors().filter_map(ElementRef::wrap))
        .any(|e| {
            e.value().attr("hidden").is_some()
                || e.value().attr("aria-hidden") == Some("true")
                || e.value()
                    .attr("style")
                    .map(|s| {
                        let s = s.replace(' ', "").to_lowercase();
                        s.contains("display:none") || s.contains("visibility:hidden")
                    })
                    .unwrap_or(false)
        })
}

fn is_focusable(el: ElementRef) -> bool {
    let v = el.value();
    if v.attr("disabled").is_some() {
        return false;
    }
    if let Some(t) = v.attr("tabindex") {
        return t.trim().parse::<i32>().map(|n| n >= 0).unwrap_or(false);
    }
    match v.name() {
        "a" | "area" => v.attr("href").is_some(),
        "button" | "select" | "textarea" | "iframe" | "summary" => true,
        "input" => v.attr("type").map(|t| !t.eq_ignore_ascii_case("hidden")).unwrap_or(true),
        _ => v.attr("contenteditable").is_some(),
    }
}

// ---------------------------------------------------------------------------
// Level A checks
// ---------------------------------------------------------------------------

fn check_images(ctx: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for img in ctx.select("img") {
        let v = img.value();
        let role = v.attr("role").unwrap_or("");
        let decorative = role == "presentation" || role == "none" || v.attr("aria-hidden") == Some("true");
        match v.attr("alt") {
            None if !decorative && attr_nonempty(img, "aria-label").is_none() && v.attr("aria-labelledby").is_none() => {
                out.push(err("Image is missing an alt attribute (use alt=\"\" if decorative).", Some(img)));
            }
            Some(alt) => {
                let a = alt.trim().to_lowercase();
                let file_like = [".jpg", ".jpeg", ".png", ".gif", ".svg", ".webp", ".bmp"]
                    .iter()
                    .any(|ext| a.ends_with(ext));
                if file_like {
                    out.push(warn("Alt text looks like a file name; describe the image instead.", Some(img)));
                } else if matches!(a.as_str(), "image" | "picture" | "photo" | "graphic" | "img" | "icon" | "logo" | "spacer") {
                    out.push(warn("Alt text is generic and probably doesn't convey the image's purpose.", Some(img)));
                }
                if a.is_empty() && has_ancestor(img, "a") {
                    // Image with empty alt inside a link: link may end up with no name.
                    if let Some(link) = img.ancestors().filter_map(ElementRef::wrap).find(|e| e.value().name() == "a") {
                        if ctx.accessible_name(link).is_empty() {
                            out.push(err("Linked image has empty alt and the link has no other text.", Some(img)));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    for el in ctx.select("input[type=image i]") {
        if attr_nonempty(el, "alt").is_none() && attr_nonempty(el, "aria-label").is_none() {
            out.push(err("Image button (input type=image) has no alt text.", Some(el)));
        }
    }
    for el in ctx.select("area[href]") {
        if attr_nonempty(el, "alt").is_none() && attr_nonempty(el, "aria-label").is_none() {
            out.push(err("Image map <area> has no alt text.", Some(el)));
        }
    }
    for el in ctx.select("svg[role=img], [role=img]:not(img):not(svg)") {
        let titled = el.children().filter_map(ElementRef::wrap).any(|c| c.value().name() == "title" && !content_text(c).is_empty());
        if !titled && ctx.accessible_name(el).is_empty() {
            out.push(err("Element with role=\"img\" has no accessible name.", Some(el)));
        }
    }
    for el in ctx.select("object, embed") {
        if ctx.accessible_name(el).is_empty() && attr_nonempty(el, "alt").is_none() {
            out.push(warn("<object>/<embed> has no text alternative.", Some(el)));
        }
    }
    out
}

fn check_media_captions(ctx: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for v in ctx.select("video") {
        let has_captions = v
            .children()
            .filter_map(ElementRef::wrap)
            .any(|c| c.value().name() == "track" && matches!(c.value().attr("kind").map(|k| k.to_lowercase()).as_deref(), Some("captions") | Some("subtitles")));
        if !has_captions {
            out.push(warn("<video> has no captions/subtitles <track>. Confirm captions are provided (1.2.2).", Some(v)));
        }
    }
    for a in ctx.select("audio") {
        out.push(warn("<audio> found: confirm a text transcript is available (1.2.1).", Some(a)));
    }
    for f in ctx.select("iframe[src*=youtube], iframe[src*=vimeo]") {
        out.push(warn("Embedded video player: confirm captions are available.", Some(f)));
    }
    out
}

fn check_form_labels(ctx: &Ctx) -> Vec<Finding> {
    let labelled_ids: Vec<String> = ctx
        .select("label[for]")
        .into_iter()
        .filter_map(|l| l.value().attr("for").map(str::to_string))
        .collect();
    let mut out = Vec::new();
    for el in ctx.select("input, select, textarea") {
        let v = el.value();
        let ty = v.attr("type").unwrap_or("text").to_lowercase();
        if matches!(ty.as_str(), "hidden" | "submit" | "reset" | "button" | "image") || is_hidden(el) {
            continue;
        }
        let by_for = v.attr("id").map(|id| labelled_ids.iter().any(|l| l == id)).unwrap_or(false);
        let wrapped = has_ancestor(el, "label");
        let aria = attr_nonempty(el, "aria-label").is_some()
            || v.attr("aria-labelledby").map(|ids| ids.split_whitespace().any(|id| ctx.ids.contains_key(id))).unwrap_or(false);
        let title = attr_nonempty(el, "title").is_some();
        if !(by_for || wrapped || aria || title) {
            let msg = if attr_nonempty(el, "placeholder").is_some() {
                "Form field has only a placeholder, which is not a substitute for a label."
            } else {
                "Form field has no associated <label>, aria-label, or aria-labelledby."
            };
            out.push(err(msg, Some(el)));
        }
    }
    out
}

fn check_heading_structure(ctx: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    let headings = ctx.select("h1, h2, h3, h4, h5, h6, [role=heading]");
    let h1s = headings.iter().filter(|h| h.value().name() == "h1").count();
    if !headings.is_empty() && h1s == 0 {
        out.push(warn("Page has headings but no <h1>.", None));
    }
    let mut prev = 0u8;
    for h in headings {
        let lvl = match h.value().name() {
            n if n.len() == 2 && n.starts_with('h') => n[1..].parse::<u8>().unwrap_or(2),
            _ => h.value().attr("aria-level").and_then(|l| l.parse().ok()).unwrap_or(2),
        };
        if prev != 0 && lvl > prev + 1 {
            out.push(warn(format!("Heading level skips from h{prev} to h{lvl}."), Some(h)));
        }
        prev = lvl;
    }
    out
}

fn check_tables(ctx: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for t in ctx.select("table") {
        let role = t.value().attr("role").unwrap_or("");
        if role == "presentation" || role == "none" {
            continue;
        }
        let rows = t.select(&sel("tr")).count();
        let has_th = t.select(&sel("th, [role=columnheader], [role=rowheader]")).next().is_some();
        if rows > 1 && !has_th {
            out.push(warn("Table has no header cells (<th>). If it is a data table, add headers; if it is for layout, add role=\"presentation\".", Some(t)));
        }
    }
    out
}

fn check_autoplay(ctx: &Ctx) -> Vec<Finding> {
    ctx.select("video[autoplay], audio[autoplay]")
        .into_iter()
        .filter(|e| e.value().attr("muted").is_none())
        .map(|e| warn("Media autoplays without being muted. If audio plays > 3s, a pause/stop/volume control is required.", Some(e)))
        .collect()
}

fn check_click_handlers(ctx: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for el in ctx.select("[onclick]") {
        let name = el.value().name();
        if matches!(name, "a" | "button" | "input" | "select" | "textarea" | "summary" | "option" | "label" | "body" | "html") {
            continue;
        }
        let has_role = el.value().attr("role").is_some();
        if !is_focusable(el) || !has_role {
            out.push(warn("Element has an onclick handler but is not a focusable control with a role; it may not be keyboard operable.", Some(el)));
        }
    }
    out
}

fn check_meta_refresh(ctx: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for m in ctx.select("meta[http-equiv]") {
        if !m.value().attr("http-equiv").unwrap_or("").eq_ignore_ascii_case("refresh") {
            continue;
        }
        let content = m.value().attr("content").unwrap_or("");
        let secs: f64 = content.split([';', ',']).next().unwrap_or("").trim().parse().unwrap_or(0.0);
        if secs > 0.0 && secs < 72_000.0 {
            out.push(err(format!("Page refreshes or redirects automatically after {secs} seconds."), Some(m)));
        }
    }
    out
}

fn check_blink_marquee(ctx: &Ctx) -> Vec<Finding> {
    ctx.select("blink, marquee")
        .into_iter()
        .map(|e| err(format!("<{}> creates moving content that users cannot pause.", e.value().name()), Some(e)))
        .collect()
}

fn check_bypass_blocks(ctx: &Ctx) -> Vec<Finding> {
    let has_main = !ctx.select("main, [role=main]").is_empty();
    let has_skip = ctx.select("a[href^='#']").into_iter().take(5).any(|a| {
        let target = &a.value().attr("href").unwrap_or("#")[1..];
        !target.is_empty() && ctx.ids.contains_key(target)
    });
    let has_headings = !ctx.select("h1, h2, h3, [role=heading]").is_empty();
    let has_nav = !ctx.select("nav, [role=navigation], header, [role=banner]").is_empty();
    if has_main || has_skip {
        Vec::new()
    } else if has_headings || has_nav {
        vec![warn("No <main> landmark or skip link found. Headings/landmarks exist, but a <main> region or skip link is recommended.", None)]
    } else {
        vec![err("No mechanism to bypass repeated content: no <main> landmark, skip link, or headings.", None)]
    }
}

fn check_page_title(ctx: &Ctx) -> Vec<Finding> {
    match ctx.select("head title, title").first() {
        None => vec![err("Page has no <title> element.", None)],
        Some(t) => {
            let text = content_text(*t);
            if text.is_empty() {
                vec![err("Page <title> is empty.", Some(*t))]
            } else if matches!(text.to_lowercase().as_str(), "untitled" | "untitled document" | "home" | "index" | "page") {
                vec![warn(format!("Page title \"{text}\" is not descriptive."), Some(*t))]
            } else {
                Vec::new()
            }
        }
    }
}

fn check_tabindex(ctx: &Ctx) -> Vec<Finding> {
    ctx.select("[tabindex]")
        .into_iter()
        .filter(|e| e.value().attr("tabindex").and_then(|t| t.trim().parse::<i32>().ok()).map(|n| n > 0).unwrap_or(false))
        .map(|e| warn("Positive tabindex overrides the natural focus order.", Some(e)))
        .collect()
}

const GENERIC_LINK_TEXT: &[&str] = &[
    "click here", "here", "click", "more", "read more", "learn more", "more info", "more information",
    "link", "this link", "this", "go", "details", "continue", "info", "see more", "view more",
];

fn check_link_purpose(ctx: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for a in ctx.select("a[href], [role=link]") {
        if is_hidden(a) {
            continue;
        }
        let name = ctx.accessible_name(a);
        if name.is_empty() {
            out.push(err("Link has no accessible name (no text, alt text, or aria-label).", Some(a)));
        } else if GENERIC_LINK_TEXT.contains(&name.to_lowercase().trim_end_matches(['.', '…', '»', '>']).trim()) {
            out.push(warn(format!("Link text \"{name}\" is ambiguous; ensure surrounding context explains its purpose."), Some(a)));
        }
    }
    out
}

fn check_label_in_name(ctx: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for el in ctx.select("a[aria-label], button[aria-label], [role=button][aria-label], [role=link][aria-label]") {
        let visible = content_text(el).to_lowercase();
        let label = el.value().attr("aria-label").unwrap_or("").to_lowercase();
        if !visible.is_empty() && visible.chars().any(char::is_alphanumeric) && !collapse_ws(&label).contains(&visible) {
            out.push(warn(format!("aria-label \"{}\" does not contain the visible text \"{}\" (speech-input users say what they see).", label.trim(), visible), Some(el)));
        }
    }
    out
}

fn valid_lang(code: &str) -> bool {
    let mut parts = code.split('-');
    let primary = parts.next().unwrap_or("");
    (2..=3).contains(&primary.len())
        && primary.chars().all(|c| c.is_ascii_alphabetic())
        && parts.all(|p| (1..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric()))
}

fn check_lang(ctx: &Ctx) -> Vec<Finding> {
    match ctx.select("html").first() {
        Some(html) => match html.value().attr("lang").map(str::trim) {
            None | Some("") => vec![err("<html> element has no lang attribute.", Some(*html))],
            Some(l) if !valid_lang(l) => vec![err(format!("<html lang=\"{l}\"> is not a valid language code."), Some(*html))],
            _ => Vec::new(),
        },
        None => vec![err("Document has no <html> element.", None)],
    }
}

fn check_buttons(ctx: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for b in ctx.select("button, [role=button], input[type=button i]") {
        if is_hidden(b) {
            continue;
        }
        let name = if b.value().name() == "input" {
            attr_nonempty(b, "value").or_else(|| attr_nonempty(b, "aria-label")).unwrap_or_default()
        } else {
            ctx.accessible_name(b)
        };
        if name.is_empty() {
            out.push(err("Button has no accessible name.", Some(b)));
        }
    }
    out
}

fn check_frames(ctx: &Ctx) -> Vec<Finding> {
    ctx.select("iframe, frame")
        .into_iter()
        .filter(|f| !is_hidden(*f))
        .filter(|f| attr_nonempty(*f, "title").is_none() && attr_nonempty(*f, "aria-label").is_none())
        .map(|f| err("Frame has no title attribute describing its content.", Some(f)))
        .collect()
}

fn check_aria_hidden_focusable(ctx: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for container in ctx.select("[aria-hidden=true]") {
        let candidates = std::iter::once(container).chain(container.descendants().filter_map(ElementRef::wrap).skip(1));
        for el in candidates {
            if is_focusable(el) {
                out.push(err("Focusable element is inside aria-hidden=\"true\"; it is reachable by keyboard but hidden from screen readers.", Some(el)));
            }
        }
    }
    out
}

const VALID_ROLES: &[&str] = &[
    "alert", "alertdialog", "application", "article", "banner", "blockquote", "button", "caption", "cell",
    "checkbox", "code", "columnheader", "combobox", "complementary", "contentinfo", "definition", "deletion",
    "dialog", "directory", "document", "emphasis", "feed", "figure", "form", "generic", "grid", "gridcell",
    "group", "heading", "img", "image", "insertion", "link", "list", "listbox", "listitem", "log", "main", "mark",
    "marquee", "math", "menu", "menubar", "menuitem", "menuitemcheckbox", "menuitemradio", "meter",
    "navigation", "none", "note", "option", "paragraph", "presentation", "progressbar", "radio",
    "radiogroup", "region", "row", "rowgroup", "rowheader", "scrollbar", "search", "searchbox",
    "separator", "slider", "spinbutton", "status", "strong", "subscript", "superscript", "switch", "tab",
    "table", "tablist", "tabpanel", "term", "textbox", "time", "timer", "toolbar", "tooltip", "tree",
    "treegrid", "treeitem", "doc-abstract", "doc-acknowledgments", "doc-afterword", "doc-appendix",
    "doc-backlink", "doc-biblioentry", "doc-bibliography", "doc-biblioref", "doc-chapter", "doc-colophon",
    "doc-conclusion", "doc-cover", "doc-credit", "doc-credits", "doc-dedication", "doc-endnote",
    "doc-endnotes", "doc-epigraph", "doc-epilogue", "doc-errata", "doc-example", "doc-footnote",
    "doc-foreword", "doc-glossary", "doc-glossref", "doc-index", "doc-introduction", "doc-noteref",
    "doc-notice", "doc-pagebreak", "doc-pagelist", "doc-part", "doc-preface", "doc-prologue",
    "doc-pullquote", "doc-qna", "doc-subtitle", "doc-tip", "doc-toc", "graphics-document",
    "graphics-object", "graphics-symbol",
];

fn check_aria_roles(ctx: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for el in ctx.select("[role]") {
        let roles = el.value().attr("role").unwrap_or("");
        let first = roles.split_whitespace().next().unwrap_or("").to_lowercase();
        if first.is_empty() {
            out.push(warn("Empty role attribute.", Some(el)));
        } else if !VALID_ROLES.contains(&first.as_str()) {
            out.push(err(format!("\"{first}\" is not a valid ARIA role."), Some(el)));
        }
    }
    out
}

/// WCAG 2.0/2.1 4.1.1 Parsing: duplicate ids are a failure (F77).
fn check_parsing_ids(ctx: &Ctx) -> Vec<Finding> {
    check_duplicate_ids(ctx)
        .into_iter()
        .map(|mut f| {
            f.severity = Severity::Error;
            f
        })
        .collect()
}

/// WCAG 2.2: 4.1.1 is obsolete, so duplicates are only an error when they break a
/// label/ARIA relationship (which then fails 1.3.1 / 4.1.2).
fn check_duplicate_ids(ctx: &Ctx) -> Vec<Finding> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for el in ctx.select("[id]") {
        if let Some(id) = el.value().attr("id") {
            *counts.entry(id).or_default() += 1;
        }
    }
    let referenced: Vec<String> = ctx
        .select("label[for], [aria-labelledby], [aria-describedby], [aria-controls]")
        .into_iter()
        .flat_map(|e| {
            ["for", "aria-labelledby", "aria-describedby", "aria-controls"]
                .iter()
                .filter_map(|a| e.value().attr(a))
                .flat_map(|v| v.split_whitespace().map(str::to_string).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        })
        .collect();
    // One finding per repeated occurrence, pointing at that element (the first use of an
    // id is the legitimate one), so each can be located in the page and the source.
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let mut out = Vec::new();
    for el in ctx.select("[id]") {
        let Some(id) = el.value().attr("id").filter(|i| !i.is_empty()) else { continue };
        let total = counts[id];
        let nth = seen.entry(id).or_default();
        *nth += 1;
        if total < 2 || *nth == 1 {
            continue;
        }
        if referenced.iter().any(|r| r == id) {
            out.push(err(
                format!("id=\"{id}\" repeats an id used earlier ({total} elements share it), and a label/ARIA attribute refers to it, so the reference is ambiguous."),
                Some(el),
            ));
        } else {
            out.push(warn(format!("id=\"{id}\" repeats an id used earlier ({total} elements share it)."), Some(el)));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Level AA checks
// ---------------------------------------------------------------------------

fn check_audio_description(ctx: &Ctx) -> Vec<Finding> {
    ctx.select("video")
        .into_iter()
        .filter(|v| {
            !v.children()
                .filter_map(ElementRef::wrap)
                .any(|c| c.value().name() == "track" && c.value().attr("kind").map(|k| k.eq_ignore_ascii_case("descriptions")).unwrap_or(false))
        })
        .map(|v| warn("<video> has no audio-description track; confirm visual information is described.", Some(v)))
        .collect()
}

fn check_autocomplete(ctx: &Ctx) -> Vec<Finding> {
    const PERSONAL: &[&str] = &[
        "email", "e-mail", "phone", "tel", "mobile", "firstname", "first_name", "first-name", "lastname",
        "last_name", "last-name", "fname", "lname", "fullname", "full_name", "address", "street", "city",
        "zip", "postal", "postcode", "country", "username", "birthday", "bday", "cc-number", "cardnumber",
    ];
    let mut out = Vec::new();
    for el in ctx.select("input:not([autocomplete])") {
        let v = el.value();
        let ty = v.attr("type").unwrap_or("text").to_lowercase();
        if matches!(ty.as_str(), "hidden" | "submit" | "button" | "reset" | "image" | "checkbox" | "radio" | "file" | "search") {
            continue;
        }
        let key = format!("{} {}", v.attr("name").unwrap_or(""), v.attr("id").unwrap_or("")).to_lowercase();
        if matches!(ty.as_str(), "email" | "tel") || PERSONAL.iter().any(|p| key.contains(p)) {
            out.push(warn("Field appears to collect personal data but has no autocomplete attribute.", Some(el)));
        }
    }
    out
}

fn check_viewport_zoom(ctx: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for m in ctx.select("meta[name=viewport i]") {
        let content = m.value().attr("content").unwrap_or("").to_lowercase().replace(' ', "");
        for part in content.split([',', ';']) {
            if let Some((k, v)) = part.split_once('=') {
                if k == "user-scalable" && (v == "no" || v == "0") {
                    out.push(err("Viewport disables pinch-zoom (user-scalable=no).", Some(m)));
                }
                if k == "maximum-scale" && v.parse::<f64>().map(|n| n < 2.0).unwrap_or(false) {
                    out.push(err(format!("Viewport limits zoom (maximum-scale={v}); users must be able to zoom to 200%."), Some(m)));
                }
            }
        }
    }
    out
}

fn check_empty_headings(ctx: &Ctx) -> Vec<Finding> {
    ctx.select("h1, h2, h3, h4, h5, h6, [role=heading]")
        .into_iter()
        .filter(|h| !is_hidden(*h) && ctx.accessible_name(*h).is_empty())
        .map(|h| err("Heading is empty.", Some(h)))
        .collect()
}

fn check_lang_parts(ctx: &Ctx) -> Vec<Finding> {
    ctx.select("body [lang]")
        .into_iter()
        .filter(|e| {
            let l = e.value().attr("lang").unwrap_or("").trim();
            !l.is_empty() && !valid_lang(l)
        })
        .map(|e| err(format!("lang=\"{}\" is not a valid language code.", e.value().attr("lang").unwrap_or("")), Some(e)))
        .collect()
}

// ---------------------------------------------------------------------------
// Colour contrast (inline styles only)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Rgb(f64, f64, f64);

fn parse_color(s: &str) -> Option<Rgb> {
    let s = s.trim().trim_end_matches("!important").trim().to_lowercase();
    if let Some(hex) = s.strip_prefix('#') {
        let h = |a: &str| u8::from_str_radix(a, 16).ok().map(f64::from);
        return match hex.len() {
            3 | 4 => {
                let d: Vec<String> = hex.chars().take(3).map(|c| format!("{c}{c}")).collect();
                Some(Rgb(h(&d[0])?, h(&d[1])?, h(&d[2])?))
            }
            6 | 8 => Some(Rgb(h(&hex[0..2])?, h(&hex[2..4])?, h(&hex[4..6])?)),
            _ => None,
        };
    }
    if s.starts_with("rgb") {
        let inner = s.split_once('(')?.1.trim_end_matches(')');
        let nums: Vec<&str> = inner.split([',', ' ', '/']).filter(|p| !p.is_empty()).collect();
        if nums.len() >= 4 {
            let alpha: f64 = nums[3].trim_end_matches('%').parse().ok()?;
            if alpha < 1.0 || nums[3].ends_with('%') && alpha < 100.0 {
                return None; // translucent – can't evaluate reliably
            }
        }
        let c = |p: &str| -> Option<f64> {
            if let Some(pct) = p.strip_suffix('%') {
                pct.parse::<f64>().ok().map(|v| v * 2.55)
            } else {
                p.parse::<f64>().ok()
            }
        };
        return Some(Rgb(c(nums.first()?)?, c(nums.get(1)?)?, c(nums.get(2)?)?));
    }
    let named = match s.as_str() {
        "black" => (0, 0, 0),
        "white" => (255, 255, 255),
        "red" => (255, 0, 0),
        "green" => (0, 128, 0),
        "blue" => (0, 0, 255),
        "yellow" => (255, 255, 0),
        "gray" | "grey" => (128, 128, 128),
        "silver" => (192, 192, 192),
        "lightgray" | "lightgrey" => (211, 211, 211),
        "darkgray" | "darkgrey" => (169, 169, 169),
        "navy" => (0, 0, 128),
        "orange" => (255, 165, 0),
        "purple" => (128, 0, 128),
        "maroon" => (128, 0, 0),
        "lime" => (0, 255, 0),
        "aqua" | "cyan" => (0, 255, 255),
        "fuchsia" | "magenta" => (255, 0, 255),
        "teal" => (0, 128, 128),
        "olive" => (128, 128, 0),
        _ => return None,
    };
    Some(Rgb(named.0 as f64, named.1 as f64, named.2 as f64))
}

fn luminance(c: Rgb) -> f64 {
    let ch = |v: f64| {
        let s = v / 255.0;
        if s <= 0.04045 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * ch(c.0) + 0.7152 * ch(c.1) + 0.0722 * ch(c.2)
}

pub fn contrast_ratio(a: (f64, f64, f64), b: (f64, f64, f64)) -> f64 {
    let (la, lb) = (luminance(Rgb(a.0, a.1, a.2)), luminance(Rgb(b.0, b.1, b.2)));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

fn style_prop(el: ElementRef, prop: &str) -> Option<String> {
    let style = el.value().attr("style")?;
    style.split(';').rev().find_map(|decl| {
        let (k, v) = decl.split_once(':')?;
        (k.trim().eq_ignore_ascii_case(prop)).then(|| v.trim().to_string())
    })
}

fn inherited<T>(el: ElementRef, f: impl Fn(ElementRef) -> Option<T>) -> Option<T> {
    std::iter::once(el).chain(el.ancestors().filter_map(ElementRef::wrap)).find_map(f)
}

fn background_of(el: ElementRef) -> Option<Rgb> {
    if let Some(c) = style_prop(el, "background-color").and_then(|v| parse_color(&v)) {
        return Some(c);
    }
    let bg = style_prop(el, "background")?;
    if bg.contains("url(") || bg.contains("gradient") {
        return None;
    }
    parse_color(&bg).or_else(|| bg.split_whitespace().find_map(parse_color))
}

fn is_large_text(el: ElementRef) -> bool {
    let size = inherited(el, |e| style_prop(e, "font-size")).unwrap_or_default().to_lowercase();
    let px = if let Some(v) = size.strip_suffix("px") {
        v.trim().parse::<f64>().ok()
    } else if let Some(v) = size.strip_suffix("pt") {
        v.trim().parse::<f64>().ok().map(|p| p * 4.0 / 3.0)
    } else if let Some(v) = size.strip_suffix("rem").or_else(|| size.strip_suffix("em")) {
        v.trim().parse::<f64>().ok().map(|e| e * 16.0)
    } else {
        None
    };
    let in_heading = matches!(el.value().name(), "h1" | "h2") || has_ancestor(el, "h1") || has_ancestor(el, "h2");
    let px = px.unwrap_or(if in_heading { 24.0 } else { 16.0 });
    let weight = inherited(el, |e| style_prop(e, "font-weight")).unwrap_or_default();
    let bold = weight == "bold" || weight == "bolder" || weight.parse::<u32>().map(|w| w >= 700).unwrap_or(false)
        || matches!(el.value().name(), "b" | "strong" | "h1" | "h2" | "h3");
    px >= 24.0 || (bold && px >= 18.66)
}

fn check_contrast(ctx: &Ctx, normal: f64, large: f64) -> Vec<Finding> {
    let mut out = Vec::new();
    for el in ctx.select("body *") {
        if matches!(el.value().name(), "script" | "style" | "noscript" | "template") || is_hidden(el) {
            continue;
        }
        let own_text: String = el
            .children()
            .filter_map(|c| match c.value() {
                Node::Text(t) => Some(t.to_string()),
                _ => None,
            })
            .collect();
        if own_text.trim().is_empty() {
            continue;
        }
        let Some(fg) = inherited(el, |e| style_prop(e, "color").and_then(|v| parse_color(&v))) else { continue };
        let Some(bg) = inherited(el, background_of) else { continue };
        let ratio = contrast_ratio((fg.0, fg.1, fg.2), (bg.0, bg.1, bg.2));
        let large_text = is_large_text(el);
        let need = if large_text { large } else { normal };
        if ratio + 1e-9 < need {
            out.push(err(
                format!(
                    "Contrast ratio {:.2}:1 is below the required {need}:1 for {} text (fg rgb({:.0},{:.0},{:.0}) on bg rgb({:.0},{:.0},{:.0})).",
                    ratio,
                    if large_text { "large" } else { "normal" },
                    fg.0, fg.1, fg.2, bg.0, bg.1, bg.2
                ),
                Some(el),
            ));
        }
    }
    out
}

fn check_contrast_aa(ctx: &Ctx) -> Vec<Finding> {
    check_contrast(ctx, 4.5, 3.0)
}

fn check_contrast_aaa(ctx: &Ctx) -> Vec<Finding> {
    check_contrast(ctx, 7.0, 4.5)
}

// ---------------------------------------------------------------------------
// Level AAA checks
// ---------------------------------------------------------------------------

fn check_link_only(ctx: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for a in ctx.select("a[href]") {
        if is_hidden(a) {
            continue;
        }
        let name = ctx.accessible_name(a);
        let norm = name.to_lowercase();
        let norm = norm.trim_end_matches(['.', '…', '»', '>']).trim();
        if !name.is_empty() && GENERIC_LINK_TEXT.contains(&norm) {
            out.push(err(format!("Link text \"{name}\" does not identify its purpose on its own."), Some(a)));
        }
    }
    out
}

fn check_section_headings(ctx: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    if ctx.select("h1, h2, h3, h4, h5, h6, [role=heading]").is_empty() {
        out.push(err("Page has no headings to organize its content.", None));
    }
    for s in ctx.select("section, article") {
        if s.select(&sel("h1, h2, h3, h4, h5, h6, [role=heading]")).next().is_none() && s.value().attr("aria-label").is_none() {
            out.push(warn(format!("<{}> has no heading.", s.value().name()), Some(s)));
        }
    }
    out
}

fn check_new_windows(ctx: &Ctx) -> Vec<Finding> {
    ctx.select("a[target=_blank]")
        .into_iter()
        .filter(|a| {
            let name = ctx.accessible_name(*a).to_lowercase();
            !(name.contains("new window") || name.contains("new tab") || name.contains("opens in"))
        })
        .map(|a| warn("Link opens a new window/tab without warning the user.", Some(a)))
        .collect()
}

// ---------------------------------------------------------------------------
// Helpers for CSS / script heuristics
// ---------------------------------------------------------------------------

fn strip_css_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        rest = match rest[start + 2..].find("*/") {
            Some(end) => &rest[start + 2 + end + 2..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// Splits compact CSS into `(selector, declarations)` pairs. Nested at-rules are
/// flattened: the selector is the text after the innermost `{`.
fn css_rules(css: &str) -> Vec<(&str, &str)> {
    css.split('}')
        .filter_map(|chunk| {
            let (selector, block) = chunk.rsplit_once('{')?;
            let selector = selector.rsplit(['{', ';']).next().unwrap_or(selector);
            Some((selector, block))
        })
        .collect()
}

/// Does the (compact, lower-case) script register a listener for `event`?
fn js_listens(js: &str, event: &str) -> bool {
    js.contains(&format!("('{event}'"))
        || js.contains(&format!("(\"{event}\""))
        || js.contains(&format!("on{event}="))
}

fn px_value(v: &str) -> Option<f64> {
    let v = v.trim().trim_end_matches("!important").trim();
    v.strip_suffix("px")?.trim().parse().ok()
}

fn style_props(el: ElementRef) -> Vec<(String, String)> {
    el.value()
        .attr("style")
        .unwrap_or("")
        .split(';')
        .filter_map(|d| d.split_once(':'))
        .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_lowercase()))
        .collect()
}

// ---------------------------------------------------------------------------
// Criteria added in WCAG 2.1
// ---------------------------------------------------------------------------

/// Looks for `key === 'k'`-style comparisons against a single printable character.
fn js_has_single_key_shortcut(js: &str) -> bool {
    ["key===", "key=="].iter().any(|pat| {
        js.match_indices(pat).any(|(i, _)| {
            let mut c = js[i + pat.len()..].chars();
            matches!((c.next(), c.next(), c.next()),
                (Some(q1), Some(ch), Some(q2)) if (q1 == '\'' || q1 == '"') && q1 == q2 && ch.is_alphanumeric())
        })
    })
}

fn check_character_shortcuts(ctx: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for el in ctx.select("[aria-keyshortcuts]") {
        for k in el.value().attr("aria-keyshortcuts").unwrap_or("").split_whitespace() {
            if !k.contains('+') && k.chars().count() == 1 {
                out.push(warn(format!("Single-character shortcut \"{k}\" has no modifier key; it must be possible to turn it off or remap it."), Some(el)));
            }
        }
    }
    if js_has_single_key_shortcut(&ctx.js) && (js_listens(&ctx.js, "keydown") || js_listens(&ctx.js, "keypress") || js_listens(&ctx.js, "keyup")) {
        out.push(warn("Inline script appears to handle single-character key presses; ensure such shortcuts can be turned off, remapped, or only work when the component has focus.", None));
    }
    out
}

fn check_pointer_gestures(ctx: &Ctx) -> Vec<Finding> {
    let mut out: Vec<Finding> = ctx
        .select("[ontouchmove], [ongesturestart], [ongesturechange]")
        .into_iter()
        .map(|e| warn("Element handles path-based or multipoint gestures; confirm a single tap/click alternative exists.", Some(e)))
        .collect();
    if ["touchmove", "gesturestart", "gesturechange"].iter().any(|e| js_listens(&ctx.js, e)) || ctx.js.contains("touches.length>1") {
        out.push(warn("Inline script handles swipe/pinch/multi-touch gestures; confirm every gesture has a single-pointer alternative.", None));
    }
    out
}

fn check_pointer_cancellation(ctx: &Ctx) -> Vec<Finding> {
    let mut out: Vec<Finding> = ctx
        .select("[onmousedown], [onpointerdown], [ontouchstart]")
        .into_iter()
        .map(|e| warn("Action is attached to a down-event; it should complete on the up-event or be abortable/undoable.", Some(e)))
        .collect();
    if ["mousedown", "pointerdown", "touchstart"].iter().any(|e| js_listens(&ctx.js, e)) {
        out.push(warn("Inline script listens for mousedown/pointerdown/touchstart; confirm actions are not triggered on the down-event.", None));
    }
    out
}

fn check_motion_actuation(ctx: &Ctx) -> Vec<Finding> {
    let mut out: Vec<Finding> = ctx
        .select("[ondevicemotion], [ondeviceorientation]")
        .into_iter()
        .map(|e| warn("Element responds to device motion; provide a UI alternative and a way to disable motion actuation.", Some(e)))
        .collect();
    if ctx.js.contains("devicemotion") || ctx.js.contains("deviceorientation") || ctx.js.contains("accelerometer") {
        out.push(warn("Inline script uses device motion/orientation; confirm functions can also be operated via UI controls and motion response can be disabled.", None));
    }
    out
}

fn check_orientation(ctx: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    if ctx.js.contains("orientation.lock(") || ctx.js.contains("lockorientation(") {
        out.push(err("Script locks the screen orientation.", None));
    }
    if ctx.css.contains("(orientation:") && ctx.css.contains("rotate(") {
        out.push(warn("CSS rotates content based on device orientation; confirm content is not restricted to one orientation.", None));
    }
    out
}

fn check_reflow(ctx: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    let viewports = ctx.select("meta[name=viewport i]");
    if viewports.is_empty() {
        out.push(warn("No viewport meta tag; on small screens the page may render at desktop width and require two-dimensional scrolling.", None));
    }
    for m in viewports {
        let content = m.value().attr("content").unwrap_or("").to_lowercase().replace(' ', "");
        if content.split([',', ';']).any(|p| p.strip_prefix("width=").map(|w| w.parse::<f64>().is_ok()).unwrap_or(false)) {
            out.push(err("Viewport is set to a fixed pixel width, preventing reflow at 320 CSS px.", Some(m)));
        }
    }
    for el in ctx.select("body [style]") {
        if matches!(el.value().name(), "img" | "video" | "canvas" | "svg" | "iframe" | "table" | "pre" | "code" | "object" | "embed" | "picture" | "map") {
            continue;
        }
        let props = style_props(el);
        if props.iter().any(|(k, _)| k == "max-width") {
            continue;
        }
        if let Some((k, v)) = props.iter().find(|(k, v)| (k == "width" || k == "min-width") && px_value(v).map(|n| n > 320.0).unwrap_or(false)) {
            out.push(warn(format!("Fixed {k}: {v} may force horizontal scrolling at a 320 CSS px viewport."), Some(el)));
        }
    }
    out
}

fn border_color(el: ElementRef) -> Option<Rgb> {
    if let Some(c) = style_prop(el, "border-color").and_then(|v| parse_color(&v)) {
        return Some(c);
    }
    let b = style_prop(el, "border")?;
    b.split_whitespace().find_map(parse_color)
}

fn check_non_text_contrast(ctx: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for el in ctx.select("input, select, textarea") {
        let ty = el.value().attr("type").unwrap_or("text").to_lowercase();
        if matches!(ty.as_str(), "hidden" | "submit" | "button" | "reset" | "image") || is_hidden(el) {
            continue;
        }
        let Some(border) = border_color(el) else { continue };
        let Some(parent) = el.parent().and_then(ElementRef::wrap) else { continue };
        let Some(outside) = inherited(parent, background_of) else { continue };
        let tuple = |c: Rgb| (c.0, c.1, c.2);
        let border_ratio = contrast_ratio(tuple(border), tuple(outside));
        let fill_ratio = background_of(el).map(|f| contrast_ratio(tuple(f), tuple(outside))).unwrap_or(1.0);
        if border_ratio < 3.0 && fill_ratio < 3.0 {
            out.push(err(format!("Form control boundary has {border_ratio:.2}:1 contrast against the adjacent background (needs 3:1)."), Some(el)));
        }
    }
    out
}

const SPACING_PROPS: &[&str] = &["line-height", "letter-spacing", "word-spacing"];

fn check_text_spacing(ctx: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for el in ctx.select("[style]") {
        let props = style_props(el);
        if let Some((k, _)) = props.iter().find(|(k, v)| SPACING_PROPS.contains(&k.as_str()) && v.contains("!important")) {
            out.push(warn(format!("Inline `{k}` uses !important, which prevents users from overriding text spacing."), Some(el)));
        }
        let fixed_height = props.iter().any(|(k, v)| k == "height" && px_value(v).is_some());
        let clipped = props.iter().any(|(k, v)| (k == "overflow" || k == "overflow-y") && v == "hidden");
        if fixed_height && clipped && !content_text(el).is_empty() {
            out.push(warn("Text container has a fixed height with overflow:hidden; text may be clipped when users increase spacing.", Some(el)));
        }
    }
    let css_important = css_rules(&ctx.css)
        .iter()
        .any(|(_, block)| block.split(';').any(|d| SPACING_PROPS.iter().any(|p| d.starts_with(&format!("{p}:"))) && d.contains("!important")));
    if css_important {
        out.push(warn("Inline <style> sets line-height/letter-spacing/word-spacing with !important, which blocks user text-spacing overrides.", None));
    }
    out
}

fn check_content_on_hover(ctx: &Ctx) -> Vec<Finding> {
    let mut out: Vec<Finding> = ctx
        .select("[role=tooltip]")
        .into_iter()
        .map(|e| warn("Custom tooltip: confirm it can be dismissed with Esc, the pointer can move onto it, and it stays visible until dismissed.", Some(e)))
        .collect();
    let reveals = css_rules(&ctx.css).iter().any(|(selector, block)| {
        selector.contains(":hover")
            && (block.contains("display:block") || block.contains("visibility:visible") || block.contains("opacity:1"))
    });
    if reveals {
        out.push(warn("CSS reveals content on :hover; confirm it is also shown on focus, is dismissible, hoverable, and persistent.", None));
    }
    out
}

const LIVE_ROLES: &[&str] = &["status", "alert", "log", "alertdialog", "progressbar", "timer", "marquee"];
const STATUS_HINTS: &[&str] = &[
    "alert", "toast", "snackbar", "notification", "flash-message", "flash", "status-message",
    "error-message", "success-message", "form-error", "validation-message", "feedback",
];

fn status_like(el: ElementRef) -> bool {
    let v = el.value();
    let tokens = format!("{} {}", v.attr("class").unwrap_or(""), v.attr("id").unwrap_or("")).to_lowercase();
    tokens.split_whitespace().any(|t| STATUS_HINTS.contains(&t) || t.ends_with("-alert") || t.ends_with("-toast"))
}

fn is_live_region(el: ElementRef) -> bool {
    let v = el.value();
    v.attr("aria-live").map(|l| l != "off").unwrap_or(false)
        || v.attr("role").map(|r| LIVE_ROLES.contains(&r.trim())).unwrap_or(false)
        || v.name() == "output"
}

fn check_status_messages(ctx: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for el in ctx.select("body [class], body [id]") {
        if matches!(el.value().name(), "a" | "button" | "input" | "select" | "textarea" | "script" | "style" | "svg" | "path" | "i") {
            continue;
        }
        if !status_like(el) {
            continue;
        }
        let mut chain = std::iter::once(el).chain(el.ancestors().filter_map(ElementRef::wrap));
        if chain.any(is_live_region) {
            continue;
        }
        // Report only the outermost matching container.
        if el.ancestors().filter_map(ElementRef::wrap).any(status_like) {
            continue;
        }
        out.push(warn("Looks like a status/alert message container but has no role=\"status\"/\"alert\" or aria-live, so updates won't be announced.", Some(el)));
    }
    out
}

fn check_identify_purpose(ctx: &Ctx) -> Vec<Finding> {
    let landmarks = ctx.select(
        "main, nav, header, footer, aside, [role=main], [role=navigation], [role=banner], \
         [role=contentinfo], [role=complementary], [role=search], [role=region], [role=form]",
    );
    if landmarks.is_empty() {
        vec![err("Page has no landmark regions, so the purpose of page regions can't be programmatically determined.", None)]
    } else if ctx.select("main, [role=main]").is_empty() {
        vec![warn("Page has landmarks but no main landmark.", None)]
    } else {
        Vec::new()
    }
}

fn check_animation_interactions(ctx: &Ctx) -> Vec<Finding> {
    let css_motion = ctx.css.contains("@keyframes")
        || css_rules(&ctx.css).iter().any(|(_, b)| {
            b.split(';').any(|d| {
                (d.starts_with("animation") || d.starts_with("transition")) && !d.ends_with(":none") && !d.ends_with(":0s")
            })
        });
    let inline_motion = ctx
        .select("[style]")
        .into_iter()
        .any(|e| style_props(e).iter().any(|(k, v)| (k.starts_with("animation") || k.starts_with("transition")) && v != "none"));
    if (css_motion || inline_motion) && !ctx.css.contains("prefers-reduced-motion") {
        vec![warn("Page defines CSS animations/transitions but no prefers-reduced-motion media query to disable non-essential motion.", None)]
    } else {
        Vec::new()
    }
}

const TARGETS: &str = "a[href], button, input:not([type=hidden]), select, textarea, summary, \
    [role=button], [role=link], [role=checkbox], [role=radio], [role=tab], [role=menuitem], [role=switch]";

fn check_target_size(ctx: &Ctx, min: f64) -> Vec<Finding> {
    let mut out = Vec::new();
    for el in ctx.select(TARGETS) {
        if is_hidden(el) {
            continue;
        }
        let props = style_props(el);
        let dim = |name: &str| props.iter().find(|(k, _)| k == name).and_then(|(_, v)| px_value(v));
        let (w, h) = (dim("width"), dim("height"));
        if w.map(|v| v < min).unwrap_or(false) || h.map(|v| v < min).unwrap_or(false) {
            let fmt = |d: Option<f64>| d.map(|v| format!("{v}")).unwrap_or_else(|| "auto".into());
            out.push(warn(
                format!("Target is {}×{} px, smaller than {min}×{min} px. Allowed exceptions: {}.",
                    fmt(w), fmt(h),
                    if min < 44.0 { "inline links, an equivalent larger target, or 24 px spacing around it" } else { "inline links or an equivalent larger target" }),
                Some(el),
            ));
        }
    }
    out
}

fn check_target_size_aaa(ctx: &Ctx) -> Vec<Finding> {
    check_target_size(ctx, 44.0)
}

// ---------------------------------------------------------------------------
// Focus visibility (2.0) and criteria added in WCAG 2.2
// ---------------------------------------------------------------------------

fn removes_outline(block: &str) -> bool {
    block.split(';').any(|d| matches!(d, "outline:none" | "outline:0" | "outline:none!important" | "outline:0!important" | "outline-style:none"))
}

fn check_focus_visible(ctx: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for (selector, block) in css_rules(&ctx.css) {
        let targets_focus = selector.split(',').any(|s| (s.contains(":focus") && !s.contains("focus-visible")) || s == "*");
        let has_replacement = ["box-shadow", "border", "background", "text-decoration", "outline-color", "outline-width"]
            .iter()
            .any(|p| block.contains(p));
        if targets_focus && removes_outline(block) && !has_replacement {
            out.push(warn(format!("CSS rule `{selector}` removes the focus outline without an alternative focus indicator."), None));
        }
    }
    for el in ctx.select("[style]") {
        let props = style_props(el);
        if is_focusable(el) && props.iter().any(|(k, v)| (k == "outline" && (v == "none" || v == "0")) || (k == "outline-style" && v == "none")) {
            out.push(warn("Inline style removes the outline on a focusable element; ensure another visible focus indicator exists.", Some(el)));
        }
    }
    out
}

fn check_focus_not_obscured(ctx: &Ctx) -> Vec<Finding> {
    let mut out: Vec<Finding> = ctx
        .select("body [style]")
        .into_iter()
        .filter(|e| !is_hidden(*e))
        .filter(|e| style_props(*e).iter().any(|(k, v)| k == "position" && (v == "fixed" || v == "sticky")))
        .map(|e| warn("Fixed/sticky element: confirm it never completely hides a keyboard-focused element (e.g. use scroll-padding).", Some(e)))
        .collect();
    if out.is_empty() && (ctx.css.contains("position:fixed") || ctx.css.contains("position:sticky")) && !ctx.css.contains("scroll-padding") {
        out.push(warn("CSS uses fixed/sticky positioning without scroll-padding; confirm focused elements are never fully hidden behind it.", None));
    }
    out
}

fn check_target_size_aa(ctx: &Ctx) -> Vec<Finding> {
    check_target_size(ctx, 24.0)
}

fn check_accessible_authentication(ctx: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    for el in ctx.select("input[onpaste], input[oncopy]") {
        let handler = format!("{}{}", el.value().attr("onpaste").unwrap_or(""), el.value().attr("oncopy").unwrap_or(""))
            .to_lowercase()
            .replace(' ', "");
        if handler.contains("returnfalse") || handler.contains("preventdefault") {
            let ty = el.value().attr("type").unwrap_or("text").to_lowercase();
            let key = format!("{} {}", el.value().attr("name").unwrap_or(""), el.value().attr("id").unwrap_or("")).to_lowercase();
            if ty == "password" || ["user", "login", "email", "otp", "code", "pin"].iter().any(|k| key.contains(k)) {
                out.push(err("Pasting is blocked on a login field, preventing password managers and copy-paste.", Some(el)));
            } else {
                out.push(warn("Pasting is blocked on this field.", Some(el)));
            }
        }
    }
    for el in ctx.select("input[type=password i][autocomplete]") {
        if el.value().attr("autocomplete").map(|a| a.eq_ignore_ascii_case("off")).unwrap_or(false) {
            out.push(warn("Password field has autocomplete=\"off\", which can stop password managers from filling it.", Some(el)));
        }
    }
    if js_listens(&ctx.js, "paste") && ctx.js.contains("preventdefault") && !ctx.select("input[type=password i]").is_empty() {
        out.push(warn("Script intercepts paste events on a page with a password field; confirm credentials can be pasted.", None));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn results(html: &str, level: Level) -> Vec<RuleResult> {
        run(&Html::parse_document(html), Version::V2_2, level)
    }

    fn results_v(html: &str, version: Version, level: Level) -> Vec<RuleResult> {
        run(&Html::parse_document(html), version, level)
    }

    fn find<'a>(r: &'a [RuleResult], title_part: &str) -> &'a RuleResult {
        r.iter().find(|x| x.title.contains(title_part)).expect("rule present")
    }

    #[test]
    fn contrast_math() {
        let r = contrast_ratio((0.0, 0.0, 0.0), (255.0, 255.0, 255.0));
        assert!((r - 21.0).abs() < 0.01);
        let r = contrast_ratio((119.0, 119.0, 119.0), (255.0, 255.0, 255.0));
        assert!((r - 4.48).abs() < 0.01);
    }

    #[test]
    fn level_filtering() {
        let a = results("<html></html>", Level::A);
        assert!(a.iter().all(|r| r.level == Level::A));
        let aaa = results("<html></html>", Level::AAA);
        assert!(aaa.len() > a.len());
    }

    #[test]
    fn detects_common_failures() {
        let html = r#"<html><head></head><body>
            <img src="a.png">
            <input type="text" id="q">
            <a href="/x"></a>
            <button></button>
            <p style="color:#777;background:#fff">low contrast</p>
        </body></html>"#;
        let r = results(html, Level::AA);
        assert_eq!(find(&r, "Non-text").outcome(), Outcome::Fail);
        assert_eq!(find(&r, "form fields").outcome(), Outcome::Fail);
        assert_eq!(find(&r, "Link Purpose").outcome(), Outcome::Fail);
        assert_eq!(find(&r, "buttons have names").outcome(), Outcome::Fail);
        assert_eq!(find(&r, "Page Titled").outcome(), Outcome::Fail);
        assert_eq!(find(&r, "Language of Page").outcome(), Outcome::Fail);
        assert_eq!(find(&r, "Contrast (Minimum)").outcome(), Outcome::Fail);
    }

    #[test]
    fn svg_names_count_toward_link_and_button_names() {
        let html = r##"<html><body>
            <a href="/"><svg role="img" aria-labelledby="t"><title id="t">W3C homepage</title></svg></a>
            <button><span><svg aria-label="Submit Search"><use href="#i"></use></svg></span></button>
        </body></html>"##;
        let r = results(html, Level::A);
        assert_eq!(find(&r, "Link Purpose").outcome(), Outcome::Pass);
        assert_eq!(find(&r, "buttons have names").outcome(), Outcome::Pass);
    }

    #[test]
    fn locator_paths() {
        let html = r#"<html><body><div></div><div><p>a</p><img src="x.png"></div></body></html>"#;
        let r = results(html, Level::A);
        let f = &find(&r, "Non-text").findings[0];
        let loc = f.locator.as_ref().expect("element finding has locator");
        assert_eq!(loc.path, "html > body:nth-of-type(1) > div:nth-of-type(2) > img:nth-of-type(1)");
        assert_eq!(loc.attr, Some(("src".into(), "x.png".into())));
        assert!(find(&r, "Page Titled").findings[0].locator.is_none());
    }

    #[test]
    fn version_filtering() {
        let count = |v| results_v("<html></html>", v, Level::AAA);
        let (v20, v21, v22) = (count(Version::V2_0), count(Version::V2_1), count(Version::V2_2));
        assert!(v20.iter().all(|r| r.since == Version::V2_0));
        assert!(v21.iter().any(|r| r.since == Version::V2_1));
        assert!(v21.iter().all(|r| r.since <= Version::V2_1));
        assert!(v22.iter().any(|r| r.since == Version::V2_2));
        assert!(v20.len() < v21.len() && v21.len() < v22.len());
        // 4.1.1 Parsing exists in 2.1 but is replaced by a best-practice check in 2.2.
        assert!(v21.iter().any(|r| r.criterion == "4.1.1"));
        assert!(!v22.iter().any(|r| r.criterion == "4.1.1"));
    }

    #[test]
    fn duplicate_ids_fail_in_21_but_not_22() {
        let html = r#"<html><body><div id="x"></div><div id="x"></div></body></html>"#;
        let r21 = results_v(html, Version::V2_1, Level::A);
        let r22 = results_v(html, Version::V2_2, Level::A);
        assert_eq!(find(&r21, "Parsing").outcome(), Outcome::Fail);
        assert_eq!(find(&r22, "best practice").outcome(), Outcome::Review);
        // The finding points at the second (repeated) element, so it can be shown in the views.
        let f = &find(&r21, "Parsing").findings;
        assert_eq!(f.len(), 1);
        let loc = f[0].locator.as_ref().expect("duplicate id finding has a locator");
        assert_eq!(loc.path, "html > body:nth-of-type(1) > div:nth-of-type(2)");
    }

    #[test]
    fn detects_wcag21_failures() {
        let html = r#"<html lang="en"><head><title>t</title>
            <meta name="viewport" content="width=1024">
            <style>a:focus { outline: none } .x { line-height: 1 !important }</style>
            <script>screen.orientation.lock('portrait');</script></head><body>
            <div style="background:#fff"><input aria-label="Name" style="border:1px solid #ddd"></div>
            <div class="toast">Saved!</div>
            <button onmousedown="go()">Go</button>
        </body></html>"#;
        let r = results_v(html, Version::V2_1, Level::AA);
        assert_eq!(find(&r, "Orientation").outcome(), Outcome::Fail);
        assert_eq!(find(&r, "Reflow").outcome(), Outcome::Fail);
        assert_eq!(find(&r, "Non-text Contrast").outcome(), Outcome::Fail);
        assert_eq!(find(&r, "Text Spacing").outcome(), Outcome::Review);
        assert_eq!(find(&r, "Status Messages").outcome(), Outcome::Review);
        assert_eq!(find(&r, "Pointer Cancellation").outcome(), Outcome::Review);
        assert_eq!(find(&r, "Focus Visible").outcome(), Outcome::Review);
        assert!(!r.iter().any(|x| x.criterion == "2.5.8"), "2.2-only rule ran under 2.1");
    }

    #[test]
    fn passes_good_markup() {
        let html = r##"<html lang="en"><head><title>Good page</title>
            <meta name="viewport" content="width=device-width, initial-scale=1"></head><body>
            <a href="#main">Skip to content</a>
            <main id="main"><h1>Hello</h1>
            <img src="a.png" alt="A red bicycle">
            <label for="q">Search</label><input type="text" id="q">
            <a href="/x">Pricing details</a>
            <button aria-label="Close dialog">Close</button>
            <p style="color:#000;background:#fff">fine</p></main>
        </body></html>"##;
        for v in Version::ALL {
            for res in &results_v(html, v, Level::AA) {
                assert_eq!(res.outcome(), Outcome::Pass, "WCAG {v} {} {}: {:?}", res.criterion, res.title, res.findings);
            }
        }
    }
}
