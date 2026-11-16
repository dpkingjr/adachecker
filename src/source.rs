//! Raw HTML source handling: a lightweight lexer for syntax colouring, start-tag byte
//! spans, display rows for the code view, and the mapping from parsed DOM elements back
//! to the start tag they came from.
//!
//! html5ever does not record source positions, so the mapping is done by aligning the
//! parsed elements (in document order) with the start tags found by the lexer, matching
//! on tag name and attributes. Elements the parser creates implicitly (e.g. `<tbody>`,
//! or `<body>` when the source omits it) simply have no source span.

use ego_tree::NodeId;
use scraper::{Html, Node};
use std::collections::HashMap;
use std::fmt;
use std::ops::Range;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    Text,
    Punct,
    TagName,
    AttrName,
    AttrValue,
    Comment,
    Doctype,
    RawText,
}

#[derive(Clone, Debug)]
pub struct TagSpan {
    /// Lower-cased tag name.
    pub name: String,
    /// Byte range of the whole start tag, `<` through `>`.
    pub range: Range<usize>,
    /// Lower-cased attribute names with their raw (undecoded) values.
    pub attrs: Vec<(String, Option<String>)>,
}

/// One display row of the code view. Long lines are split into several rows.
#[derive(Clone, Debug)]
pub struct Row {
    /// 0-based source line number.
    pub line: usize,
    /// First row of its line (shows the line number).
    pub first: bool,
    pub range: Range<usize>,
}

pub struct SourceDoc {
    pub text: String,
    pub classes: Vec<Class>,
    pub tags: Vec<TagSpan>,
    /// Byte offset where each line starts.
    line_starts: Vec<usize>,
}

impl fmt::Debug for SourceDoc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SourceDoc({} bytes, {} lines, {} tags)", self.text.len(), self.line_starts.len(), self.tags.len())
    }
}


/// Elements whose content is raw text (no child tags) when html5ever parses with
/// scripting enabled (its default, used by scraper).
const RAW_TEXT: &[&str] = &["script", "style", "textarea", "title", "xmp", "iframe", "noembed", "noframes", "noscript"];

/// Elements html5ever may create without a source tag.
const IMPLICIT: &[&str] = &["html", "head", "body", "tbody", "colgroup"];

impl SourceDoc {
    pub fn new(text: String) -> Self {
        let (classes, tags) = lex(&text);
        let line_starts = std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect();
        SourceDoc { text, classes, tags, line_starts }
    }

    /// 0-based line containing byte `offset`.
    pub fn line_of(&self, offset: usize) -> usize {
        self.line_starts.partition_point(|&s| s <= offset).saturating_sub(1)
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// Splits the source into display rows of at most `wrap` characters.
    pub fn wrap_rows(&self, wrap: usize) -> Vec<Row> {
        build_rows(&self.text, wrap.max(1))
    }
}

/// Index of the display row containing byte `offset`.
pub fn row_of(rows: &[Row], offset: usize) -> usize {
    rows.partition_point(|r| r.range.start <= offset).saturating_sub(1)
}

fn is_ws(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')
}

fn find(b: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    if from >= b.len() {
        return None;
    }
    b[from..].windows(pat.len()).position(|w| w == pat).map(|p| p + from)
}

fn find_byte(b: &[u8], from: usize, c: u8) -> Option<usize> {
    b.get(from..)?.iter().position(|&x| x == c).map(|p| p + from)
}

/// Finds `</name` (ASCII case-insensitive) at or after `from`.
fn find_close(b: &[u8], from: usize, name: &str) -> Option<usize> {
    let pat = name.as_bytes();
    let mut i = from;
    while let Some(lt) = find_byte(b, i, b'<') {
        let s = lt + 2;
        if b.get(lt + 1) == Some(&b'/') && b.len() >= s + pat.len() && b[s..s + pat.len()].eq_ignore_ascii_case(pat) {
            let after = b.get(s + pat.len()).copied();
            if after.is_none_or(|c| is_ws(c) || c == b'>' || c == b'/') {
                return Some(lt);
            }
        }
        i = lt + 1;
    }
    None
}

fn fill(cls: &mut [Class], r: Range<usize>, c: Class) {
    let end = r.end.min(cls.len());
    if r.start < end {
        cls[r.start..end].fill(c);
    }
}

/// Lexes a start tag beginning at `i` (`b[i] == '<'`). Returns the tag and its end.
fn lex_start_tag(src: &str, i: usize, cls: &mut [Class]) -> TagSpan {
    let b = src.as_bytes();
    let n = b.len();
    cls[i] = Class::Punct;
    let mut j = i + 1;
    while j < n && !is_ws(b[j]) && b[j] != b'/' && b[j] != b'>' {
        j += 1;
    }
    fill(cls, i + 1..j, Class::TagName);
    let name = src[i + 1..j].to_ascii_lowercase();
    let mut attrs = Vec::new();
    loop {
        while j < n && (is_ws(b[j]) || b[j] == b'/') {
            if b[j] == b'/' {
                cls[j] = Class::Punct;
            }
            j += 1;
        }
        if j >= n {
            break;
        }
        if b[j] == b'>' {
            cls[j] = Class::Punct;
            j += 1;
            break;
        }
        // Attribute name (a leading '=' is part of the name per the HTML spec).
        let start = j;
        j += 1;
        while j < n && !is_ws(b[j]) && !matches!(b[j], b'/' | b'>' | b'=') {
            j += 1;
        }
        fill(cls, start..j, Class::AttrName);
        let attr_name = src[start..j].to_ascii_lowercase();
        let mut k = j;
        while k < n && is_ws(b[k]) {
            k += 1;
        }
        let mut value = None;
        if k < n && b[k] == b'=' {
            cls[k] = Class::Punct;
            k += 1;
            while k < n && is_ws(b[k]) {
                k += 1;
            }
            if k < n && (b[k] == b'"' || b[k] == b'\'') {
                let close = find_byte(b, k + 1, b[k]).unwrap_or(n);
                fill(cls, k..(close + 1).min(n), Class::AttrValue);
                value = Some(src[k + 1..close].to_string());
                j = (close + 1).min(n);
            } else {
                let vs = k;
                while k < n && !is_ws(b[k]) && b[k] != b'>' {
                    k += 1;
                }
                fill(cls, vs..k, Class::AttrValue);
                value = Some(src[vs..k].to_string());
                j = k;
            }
        }
        attrs.push((attr_name, value));
    }
    TagSpan { name, range: i..j, attrs }
}

pub fn lex(src: &str) -> (Vec<Class>, Vec<TagSpan>) {
    let b = src.as_bytes();
    let n = b.len();
    let mut cls = vec![Class::Text; n];
    let mut tags = Vec::new();
    let mut i = 0;
    while let Some(lt) = find_byte(b, i, b'<') {
        i = lt;
        let next = b.get(i + 1).copied();
        if b[i..].starts_with(b"<!--") {
            let end = find(b, i + 4, b"-->").map(|e| e + 3).unwrap_or(n);
            fill(&mut cls, i..end, Class::Comment);
            i = end;
        } else if b[i..].starts_with(b"<![CDATA[") {
            let end = find(b, i, b"]]>").map(|e| e + 3).unwrap_or(n);
            fill(&mut cls, i..end, Class::Doctype);
            i = end;
        } else if matches!(next, Some(b'!') | Some(b'?')) {
            let end = find_byte(b, i, b'>').map(|e| e + 1).unwrap_or(n);
            fill(&mut cls, i..end, Class::Doctype);
            i = end;
        } else if next == Some(b'/') && b.get(i + 2).is_some_and(|c| c.is_ascii_alphabetic()) {
            let end = find_byte(b, i, b'>').map(|e| e + 1).unwrap_or(n);
            fill(&mut cls, i..end, Class::Punct);
            let mut k = i + 2;
            while k < end && !is_ws(b[k]) && b[k] != b'>' {
                k += 1;
            }
            fill(&mut cls, i + 2..k, Class::TagName);
            i = end;
        } else if next.is_some_and(|c| c.is_ascii_alphabetic()) {
            let tag = lex_start_tag(src, i, &mut cls);
            i = tag.range.end;
            let name = tag.name.clone();
            tags.push(tag);
            if name == "plaintext" {
                fill(&mut cls, i..n, Class::RawText);
                break;
            }
            if RAW_TEXT.contains(&name.as_str()) {
                let close = find_close(b, i, &name).unwrap_or(n);
                fill(&mut cls, i..close, Class::RawText);
                i = close;
            }
        } else {
            i += 1;
        }
    }
    (cls, tags)
}

fn build_rows(text: &str, wrap: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut line_start = 0;
    for (line, raw) in text.split('\n').enumerate() {
        let content = raw.strip_suffix('\r').unwrap_or(raw);
        let mut seg_start = line_start;
        let mut count = 0;
        let mut first = true;
        for (off, _) in content.char_indices() {
            if count == wrap {
                rows.push(Row { line, first, range: seg_start..line_start + off });
                first = false;
                seg_start = line_start + off;
                count = 0;
            }
            count += 1;
        }
        rows.push(Row { line, first, range: seg_start..line_start + content.len() });
        line_start += raw.len() + 1;
    }
    rows
}

fn attrs_match(el: &scraper::node::Element, tag: &TagSpan) -> bool {
    // The parser keeps only the first occurrence of a duplicated attribute.
    let mut seen: Vec<&str> = Vec::new();
    let mut src_attrs = Vec::new();
    for (k, v) in &tag.attrs {
        let local = k.rsplit(':').next().unwrap_or(k);
        if !seen.contains(&local) {
            seen.push(local);
            src_attrs.push((local, v.as_deref()));
        }
    }
    if el.attrs().count() != src_attrs.len() {
        return false;
    }
    src_attrs.iter().all(|(name, raw)| {
        el.attrs().any(|(dk, dv)| {
            dk.eq_ignore_ascii_case(name) && match raw {
                // Entities are decoded in the DOM, so only compare plain values exactly.
                Some(r) if !r.contains('&') => *r == dv,
                _ => true,
            }
        })
    })
}

/// Maps each parsed element to the byte range of its source start tag.
pub fn map_elements(doc: &Html, tags: &[TagSpan]) -> HashMap<NodeId, Range<usize>> {
    const LOOKAHEAD: usize = 64;
    let mut map = HashMap::new();
    let mut j = 0;
    for node in doc.tree.root().descendants() {
        let Node::Element(el) = node.value() else { continue };
        let name = el.name().to_ascii_lowercase();
        // Implicit-capable elements only match the very next source tag; otherwise an
        // implied <tbody> could steal a later explicit one and derail the alignment.
        let end = if IMPLICIT.contains(&name.as_str()) { (j + 1).min(tags.len()) } else { (j + LOOKAHEAD).min(tags.len()) };
        if let Some(k) = (j..end).find(|&k| tags[k].name == name && attrs_match(el, &tags[k])) {
            map.insert(node.id(), tags[k].range.clone());
            j = k + 1;
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use scraper::Selector;

    fn span_of<'a>(src: &'a str, doc: &Html, map: &HashMap<NodeId, Range<usize>>, css: &str) -> Option<&'a str> {
        let el = doc.select(&Selector::parse(css).unwrap()).next().unwrap();
        map.get(&el.id()).map(|r| &src[r.clone()])
    }

    #[test]
    fn lexes_tags_and_skips_raw_text() {
        let src = r#"<!doctype html><script>if (a<b) { x = "<div>"; }</script><p class="x" hidden>hi</p><!-- <span> -->"#;
        let (cls, tags) = lex(src);
        let names: Vec<_> = tags.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["script", "p"]);
        assert_eq!(&src[tags[1].range.clone()], r#"<p class="x" hidden>"#);
        assert_eq!(tags[1].attrs, vec![("class".into(), Some("x".into())), ("hidden".into(), None)]);
        let p = tags[1].range.start;
        assert_eq!(cls[p + 1], Class::TagName);
        assert_eq!(cls[p + 3], Class::AttrName);
        assert_eq!(cls[src.find("\"x\"").unwrap()], Class::AttrValue);
        assert_eq!(cls[src.find("<div>").unwrap()], Class::RawText);
        assert_eq!(cls[src.find("<span>").unwrap()], Class::Comment);
    }

    #[test]
    fn maps_elements_with_implicit_parents() {
        let src = "<title>T</title>\n<table><tr><td id=a>1</td></tr></table>\n<TABLE><tbody><tr><td>2</td></tr></tbody></TABLE>\n<img src=\"x.png\" alt=\"a &amp; b\">";
        let doc = Html::parse_document(src);
        let (_, tags) = lex(src);
        let map = map_elements(&doc, &tags);
        assert_eq!(span_of(src, &doc, &map, "td#a"), Some("<td id=a>"));
        assert_eq!(span_of(src, &doc, &map, "table:nth-of-type(2) tbody"), Some("<tbody>"));
        assert_eq!(span_of(src, &doc, &map, "table:nth-of-type(2) td"), Some("<td>"));
        assert_eq!(span_of(src, &doc, &map, "img"), Some("<img src=\"x.png\" alt=\"a &amp; b\">"));
        assert_eq!(span_of(src, &doc, &map, "body"), None, "implicit body has no source tag");
        assert_eq!(span_of(src, &doc, &map, "table:nth-of-type(1) tbody"), None, "implied tbody");
    }

    /// `SCAN_FILE=page.html cargo test alignment_coverage -- --ignored --nocapture`
    #[test]
    #[ignore = "needs SCAN_FILE"]
    fn alignment_coverage() {
        let src = std::fs::read_to_string(std::env::var("SCAN_FILE").unwrap()).unwrap();
        let doc = Html::parse_document(&src);
        let (_, tags) = lex(&src);
        let map = map_elements(&doc, &tags);
        let elements = doc.tree.root().descendants().filter(|n| n.value().is_element()).count();
        let wrong = map.iter().filter(|(id, r)| {
            let name = doc.tree.get(**id).unwrap().value().as_element().unwrap().name().to_ascii_lowercase();
            !src[(*r).clone()].to_ascii_lowercase().starts_with(&format!("<{name}"))
        }).count();
        println!("elements={elements} source_tags={} mapped={} wrong_name={wrong}", tags.len(), map.len());
        assert_eq!(wrong, 0);
    }

    #[test]
    fn wraps_long_lines_into_rows() {
        const WRAP: usize = 50;
        let long = "é".repeat(WRAP * 2 + 5);
        let doc = SourceDoc::new(format!("a\r\n{long}\nb"));
        let rows = doc.wrap_rows(WRAP);
        assert_eq!(rows.len(), 1 + 3 + 1);
        assert!(rows[1].first && !rows[2].first);
        assert_eq!(rows[2].line, 1);
        assert_eq!(rows[0].range, 0..1, "CR stripped");
        assert_eq!(doc.text[rows[1].range.clone()].chars().count(), WRAP, "wraps by chars, not bytes");
        assert_eq!(row_of(&rows, doc.text.len() - 1), 4);
        assert_eq!(doc.line_count(), 3);
        assert_eq!(doc.line_of(doc.text.len() - 1), 2);
    }
}
