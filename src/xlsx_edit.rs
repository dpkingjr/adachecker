//! Marks worksheet rows of an `.xlsx`/`.xlsm` workbook with a status fill colour by editing
//! the workbook's XML in place.
//!
//! Only two kinds of part are changed: `xl/styles.xml` (three fills and the cell formats
//! that use them are appended) and the worksheets (status rows point at those formats).
//! Every other part of the zip is copied byte-for-byte, so formatting, formulas, column
//! widths, comments etc. survive. Saving always starts from the workbook as it was opened,
//! so repeated saves don't accumulate styles, and clearing a status removes its fill.

use std::collections::{BTreeMap, HashMap};
use std::io::{Cursor, Read, Write};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Status {
    Fixed,
    OnHold,
    NotApplicable,
    Broken,
}

impl Status {
    pub const ALL: [Status; 4] = [Status::Fixed, Status::OnHold, Status::NotApplicable, Status::Broken];

    pub fn label(self) -> &'static str {
        match self {
            Status::Fixed => "Fixed",
            Status::OnHold => "On-Hold",
            Status::NotApplicable => "N/A",
            Status::Broken => "Broken",
        }
    }

    /// Fill colour written to the spreadsheet (ARGB): Excel's "Good" green, "Neutral"
    /// yellow, a light grey, and Excel's "Bad" red — all readable with black text.
    pub fn argb(self) -> &'static str {
        match self {
            Status::Fixed => "FFC6EFCE",
            Status::OnHold => "FFFFEB9C",
            Status::NotApplicable => "FFD9D9D9",
            Status::Broken => "FFFFC7CE",
        }
    }

    fn from_argb(rgb: &str) -> Option<Status> {
        Status::ALL.into_iter().find(|s| s.argb().eq_ignore_ascii_case(rgb))
    }
}

/// Worksheet name → (1-based Excel row number → status).
pub type Statuses = HashMap<String, BTreeMap<u32, Status>>;

/// `report.xlsx` → `report-fixed.xlsx` in the same folder. A file that already ends in
/// `-fixed` is saved in place.
pub fn fixed_path(path: &Path) -> std::path::PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    if stem.ends_with("-fixed") {
        return path.to_path_buf();
    }
    let name = match path.extension() {
        Some(ext) => format!("{stem}-fixed.{}", ext.to_string_lossy()),
        None => format!("{stem}-fixed"),
    };
    path.with_file_name(name)
}

/// Whether statuses can be saved to this kind of file.
pub fn is_editable(path: &Path) -> bool {
    path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).is_some_and(|e| e == "xlsx" || e == "xlsm")
}

// ---------------------------------------------------------------------------
// Minimal XML tag scanning
// ---------------------------------------------------------------------------

struct Tag<'a> {
    start: usize,
    end: usize,
    text: &'a str,
    /// Qualified name, e.g. `row` or `x:row`.
    name: &'a str,
    closing: bool,
    self_closing: bool,
}

impl Tag<'_> {
    fn local(&self) -> &str {
        self.name.rsplit(':').next().unwrap_or(self.name)
    }
    /// Namespace prefix including the colon (`"x:"`), or `""`.
    fn prefix(&self) -> &str {
        self.name.rfind(':').map(|i| &self.name[..=i]).unwrap_or("")
    }
}

/// Next element tag at or after `from`, skipping comments, CDATA, declarations and PIs.
fn next_tag(xml: &str, from: usize) -> Option<Tag<'_>> {
    let b = xml.as_bytes();
    let mut i = from;
    loop {
        let lt = xml.get(i..)?.find('<')? + i;
        let rest = &xml[lt..];
        if rest.starts_with("<!--") {
            i = rest.find("-->")? + lt + 3;
            continue;
        }
        if rest.starts_with("<![CDATA[") {
            i = rest.find("]]>")? + lt + 3;
            continue;
        }
        if rest.starts_with("<?") || rest.starts_with("<!") {
            i = rest.find('>')? + lt + 1;
            continue;
        }
        let mut j = lt + 1;
        let mut quote = None;
        while j < b.len() {
            let c = b[j];
            match quote {
                Some(q) if c == q => quote = None,
                Some(_) => {}
                None if c == b'"' || c == b'\'' => quote = Some(c),
                None if c == b'>' => break,
                None => {}
            }
            j += 1;
        }
        if j >= b.len() {
            return None;
        }
        let text = &xml[lt..=j];
        let closing = text.starts_with("</");
        let self_closing = text.ends_with("/>");
        let ns = if closing { 2 } else { 1 };
        let ne = text[ns..].find(|c: char| c.is_whitespace() || c == '>' || c == '/').map(|k| k + ns).unwrap_or(text.len() - 1);
        return Some(Tag { start: lt, end: j + 1, text, name: &text[ns..ne], closing, self_closing });
    }
}

/// Attributes of a start tag: (name, raw value, value byte range within the tag).
fn attrs(tag: &str) -> Vec<(&str, &str, std::ops::Range<usize>)> {
    let b = tag.as_bytes();
    let mut out = Vec::new();
    // Skip "<name".
    let mut i = tag.find(|c: char| c.is_whitespace()).unwrap_or(tag.len());
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let ns = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' && b[i] != b'>' && b[i] != b'/' {
            i += 1;
        }
        let name = &tag[ns..i];
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if name.is_empty() || i >= b.len() || b[i] != b'=' {
            i += 1;
            continue;
        }
        i += 1;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let Some(&q) = b.get(i) else { break };
        if q != b'"' && q != b'\'' {
            break;
        }
        let vs = i + 1;
        let Some(ve) = tag[vs..].find(q as char).map(|k| k + vs) else { break };
        out.push((name, &tag[vs..ve], vs..ve));
        i = ve + 1;
    }
    out
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    attrs(tag).into_iter().find(|(n, _, _)| *n == name).map(|(_, v, _)| v)
}

/// Sets (or adds) attribute `name` on the start tag `tag`.
fn set_attr(tag: &str, name: &str, value: &str) -> String {
    if let Some((_, _, range)) = attrs(tag).into_iter().find(|(n, _, _)| *n == name) {
        return format!("{}{value}{}", &tag[..range.start], &tag[range.end..]);
    }
    let insert_at = if tag.ends_with("/>") { tag.len() - 2 } else { tag.len() - 1 };
    format!("{} {name}=\"{value}\"{}", tag[..insert_at].trim_end(), &tag[insert_at..])
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// Finds the first start tag with local name `local` at or after `from`.
fn find_start<'a>(xml: &'a str, from: usize, local: &str) -> Option<Tag<'a>> {
    let mut i = from;
    while let Some(t) = next_tag(xml, i) {
        if !t.closing && t.local() == local {
            return Some(t);
        }
        i = t.end;
    }
    None
}

/// Finds the closing tag with local name `local` at or after `from`.
fn find_end<'a>(xml: &'a str, from: usize, local: &str) -> Option<Tag<'a>> {
    let mut i = from;
    while let Some(t) = next_tag(xml, i) {
        if t.closing && t.local() == local {
            return Some(t);
        }
        i = t.end;
    }
    None
}

// ---------------------------------------------------------------------------
// styles.xml
// ---------------------------------------------------------------------------

struct Styles {
    xml: String,
    prefix: String,
    /// Number of `<fill>` elements already in the file.
    fills: usize,
    /// Existing fills that carry one of our status colours (from an earlier save).
    status_fills: HashMap<usize, Status>,
    new_fills: Vec<Status>,
    /// Raw `<xf …>` elements of `<cellXfs>`, and each one's fillId.
    xfs: Vec<String>,
    xf_fill: Vec<usize>,
    new_xfs: Vec<String>,
    variants: HashMap<(usize, usize), usize>,
}

impl Styles {
    fn parse(xml: String) -> Result<Styles, String> {
        let fills_tag = find_start(&xml, 0, "fills").ok_or("styles.xml has no <fills> section")?;
        let prefix = fills_tag.prefix().to_string();
        let mut fills = 0;
        let mut status_fills = HashMap::new();
        if !fills_tag.self_closing {
            let mut i = fills_tag.end;
            while let Some(t) = next_tag(&xml, i) {
                if t.closing && t.local() == "fills" {
                    break;
                }
                if !t.closing && t.local() == "fill" {
                    fills += 1;
                }
                if !t.closing && t.local() == "fgColor" && fills > 0 {
                    if let Some(status) = attr(t.text, "rgb").and_then(Status::from_argb) {
                        status_fills.entry(fills - 1).or_insert(status);
                    }
                }
                i = t.end;
            }
        }
        let xfs_tag = find_start(&xml, 0, "cellXfs").ok_or("styles.xml has no <cellXfs> section")?;
        let mut xfs = Vec::new();
        let mut xf_fill = Vec::new();
        let mut i = xfs_tag.end;
        while let Some(t) = next_tag(&xml, i) {
            if t.closing && t.local() == "cellXfs" {
                break;
            }
            if !t.closing && t.local() == "xf" {
                let end = if t.self_closing { t.end } else { find_end(&xml, t.end, "xf").ok_or("unterminated <xf>")?.end };
                xfs.push(xml[t.start..end].to_string());
                xf_fill.push(attr(t.text, "fillId").and_then(|v| v.parse().ok()).unwrap_or(0));
                i = end;
                continue;
            }
            i = t.end;
        }
        if xfs.is_empty() {
            return Err("styles.xml has no cell formats".into());
        }
        Ok(Styles { xml, prefix, fills, status_fills, new_fills: Vec::new(), xfs, xf_fill, new_xfs: Vec::new(), variants: HashMap::new() })
    }

    fn fill_for(&mut self, status: Status) -> usize {
        if let Some((&id, _)) = self.status_fills.iter().filter(|(_, s)| **s == status).min_by_key(|(id, _)| **id) {
            return id;
        }
        if let Some(pos) = self.new_fills.iter().position(|s| *s == status) {
            return self.fills + pos;
        }
        self.new_fills.push(status);
        self.fills + self.new_fills.len() - 1
    }

    fn fill_of(&self, xf: usize) -> usize {
        let total = self.xf_fill.len();
        if xf < total {
            self.xf_fill[xf]
        } else {
            // A format we added: read back its fillId.
            self.new_xfs.get(xf - total).and_then(|raw| attr(raw, "fillId")).and_then(|v| v.parse().ok()).unwrap_or(0)
        }
    }

    /// Status of a cell format whose fill is one of our status colours.
    fn status_of_xf(&self, xf: usize) -> Option<Status> {
        let fill = self.fill_of(xf);
        self.status_fills.get(&fill).copied().or_else(|| {
            fill.checked_sub(self.fills).and_then(|i| self.new_fills.get(i)).copied()
        })
    }

    /// The format like `xf` but with the fill for `desired` — or, for `None`, with any
    /// status fill removed (other fills are left alone).
    fn variant(&mut self, xf: usize, desired: Option<Status>) -> usize {
        let xf = if xf < self.xf_fill.len() + self.new_xfs.len() { xf } else { 0 };
        let target = match desired {
            Some(status) => self.fill_for(status),
            None if self.status_of_xf(xf).is_some() => 0,
            None => return xf,
        };
        if self.fill_of(xf) == target {
            return xf;
        }
        if let Some(&v) = self.variants.get(&(xf, target)) {
            return v;
        }
        let raw = if xf < self.xfs.len() { self.xfs[xf].clone() } else { self.new_xfs[xf - self.xfs.len()].clone() };
        let first = next_tag(&raw, 0).expect("xf tag");
        let mut tag = set_attr(first.text, "fillId", &target.to_string());
        tag = set_attr(&tag, "applyFill", "1");
        let cloned = format!("{tag}{}", &raw[first.end..]);
        self.new_xfs.push(cloned);
        let id = self.xfs.len() + self.new_xfs.len() - 1;
        self.variants.insert((xf, target), id);
        id
    }

    fn finish(self) -> String {
        let p = &self.prefix;
        let mut xml = self.xml;
        // cellXfs comes after fills; edit it first so the fills offsets stay valid.
        if !self.new_xfs.is_empty() {
            let open = find_start(&xml, 0, "cellXfs").expect("cellXfs");
            let close = find_end(&xml, open.end, "cellXfs").expect("cellXfs end");
            let count = (self.xfs.len() + self.new_xfs.len()).to_string();
            let new_open = set_attr(open.text, "count", &count);
            let (os, oe, cs) = (open.start, open.end, close.start);
            xml = format!("{}{new_open}{}{}{}", &xml[..os], &xml[oe..cs], self.new_xfs.concat(), &xml[cs..]);
        }
        if !self.new_fills.is_empty() {
            let open = find_start(&xml, 0, "fills").expect("fills");
            let new_fills: String = self
                .new_fills
                .iter()
                .map(|s| {
                    format!(
                        "<{p}fill><{p}patternFill patternType=\"solid\"><{p}fgColor rgb=\"{}\"/><{p}bgColor indexed=\"64\"/></{p}patternFill></{p}fill>",
                        s.argb()
                    )
                })
                .collect();
            let count = (self.fills + self.new_fills.len()).to_string();
            if open.self_closing {
                let replacement = format!("<{p}fills count=\"{count}\">{new_fills}</{p}fills>");
                xml = format!("{}{replacement}{}", &xml[..open.start], &xml[open.end..]);
            } else {
                let close = find_end(&xml, open.end, "fills").expect("fills end");
                let new_open = set_attr(open.text, "count", &count);
                let (os, oe, cs) = (open.start, open.end, close.start);
                xml = format!("{}{new_open}{}{new_fills}{}", &xml[..os], &xml[oe..cs], &xml[cs..]);
            }
        }
        xml
    }
}

// ---------------------------------------------------------------------------
// Worksheets
// ---------------------------------------------------------------------------

/// 0-based column index from a cell reference such as `"C12"`.
fn col_of(cell_ref: &str) -> Option<u32> {
    let letters: String = cell_ref.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    if letters.is_empty() {
        return None;
    }
    Some(letters.bytes().fold(0u32, |acc, b| acc * 26 + u32::from(b.to_ascii_uppercase() - b'A' + 1)) - 1)
}

/// 1-based row number from a cell reference such as `"C12"`.
fn row_of(cell_ref: &str) -> Option<u32> {
    cell_ref.trim_start_matches(|c: char| c.is_ascii_alphabetic()).parse().ok()
}

fn col_letters(mut i: u32) -> String {
    let mut s = Vec::new();
    loop {
        s.push(b'A' + (i % 26) as u8);
        if i < 26 {
            break;
        }
        i = i / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).expect("ASCII")
}

/// Escapes text for XML content, dropping characters XML can't contain.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

/// The sheet's used range from `<dimension ref="A1:I29"/>`: (first row, first col, last row,
/// last col), rows 1-based and columns 0-based.
fn dimension(xml: &str) -> Option<(u32, u32, u32, u32)> {
    let dim = find_start(xml, 0, "dimension")?;
    let r = attr(dim.text, "ref")?;
    let (a, b) = r.split_once(':').unwrap_or((r, r));
    Some((row_of(a)?, col_of(a)?, row_of(b)?, col_of(b)?))
}

/// Text of every shared string (`xl/sharedStrings.xml`), ignoring phonetic runs.
fn shared_strings(zip: &mut Archive) -> Vec<String> {
    let Ok(xml) = read_part(zip, "xl/sharedStrings.xml") else { return Vec::new() };
    let mut out = Vec::new();
    let mut current: Option<String> = None;
    let mut in_phonetic = false;
    let mut i = 0;
    while let Some(t) = next_tag(&xml, i) {
        i = t.end;
        match (t.local(), t.closing) {
            ("si", false) if !t.self_closing => current = Some(String::new()),
            ("si", false) => out.push(String::new()),
            ("si", true) => out.push(current.take().unwrap_or_default()),
            ("rPh", false) if !t.self_closing => in_phonetic = true,
            ("rPh", true) => in_phonetic = false,
            ("t", false) if !t.self_closing && !in_phonetic => {
                if let Some(close) = find_end(&xml, t.end, "t") {
                    if let Some(cur) = current.as_mut() {
                        cur.push_str(&unescape(&xml[t.end..close.start]));
                    }
                    i = close.end;
                }
            }
            _ => {}
        }
    }
    out
}

/// A cell's displayed text (shared string, inline string, or plain value).
fn cell_text(raw: &str, shared: &[String]) -> String {
    let Some(open) = next_tag(raw, 0) else { return String::new() };
    let inner_text = |local: &str| -> String {
        let mut s = String::new();
        let mut i = open.end;
        while let Some(t) = next_tag(raw, i) {
            i = t.end;
            if !t.closing && !t.self_closing && t.local() == local {
                if let Some(close) = find_end(raw, t.end, local) {
                    s.push_str(&unescape(&raw[t.end..close.start]));
                    i = close.end;
                }
            }
        }
        s
    };
    match attr(open.text, "t") {
        Some("s") => inner_text("v").trim().parse::<usize>().ok().and_then(|n| shared.get(n).cloned()).unwrap_or_default(),
        Some("inlineStr") => inner_text("t"),
        _ => inner_text("v"),
    }
}

/// Where the Notes column goes on a worksheet.
#[derive(Clone, Copy, Debug)]
struct NotesPlan {
    /// 0-based column.
    col: u32,
    /// 1-based header row (the first row of the used range).
    header_row: u32,
    /// The sheet has no "Notes" header yet: add one.
    add_header: bool,
}

/// Worksheet name → (1-based Excel row number → note text).
pub type Notes = HashMap<String, BTreeMap<u32, String>>;

/// The header text of the notes column.
pub const NOTES_HEADER: &str = "Notes";

fn plan_notes(xml: &str, shared: &[String]) -> Option<NotesPlan> {
    let (first_row, _, _, last_col) = dimension(xml)?;
    let data = find_start(xml, 0, "sheetData")?;
    if data.self_closing {
        return None;
    }
    // Look for an existing "Notes" header in the header row.
    let mut i = data.end;
    while let Some(t) = next_tag(xml, i) {
        if t.closing && t.local() == "sheetData" {
            break;
        }
        if !t.closing && t.local() == "row" {
            let r = attr(t.text, "r").and_then(|v| v.parse::<u32>().ok());
            if r == Some(first_row) && !t.self_closing {
                let (cells, _, _) = parse_row(xml, &t, 0).ok()?;
                if let Some((col, _)) = cells.iter().find(|(_, raw)| cell_text(raw, shared).trim().eq_ignore_ascii_case(NOTES_HEADER)) {
                    return Some(NotesPlan { col: *col, header_row: first_row, add_header: false });
                }
                break;
            }
            if r.is_some_and(|r| r > first_row) {
                break;
            }
        }
        i = t.end;
    }
    Some(NotesPlan { col: last_col + 1, header_row: first_row, add_header: true })
}

/// The cells of a non-self-closing `<row>`: (column, raw element), any other child
/// elements, and the byte offset just past `</row>`.
fn parse_row(xml: &str, open: &Tag, first_col: u32) -> Result<(Vec<(u32, String)>, String, usize), String> {
    let close = find_end(xml, open.end, "row").ok_or("unterminated <row>")?;
    let mut cells = Vec::new();
    let mut others = String::new();
    let mut i = open.end;
    let mut next_col = first_col;
    while let Some(ct) = next_tag(xml, i) {
        if ct.start >= close.start {
            break;
        }
        let end = if ct.self_closing || ct.closing { ct.end } else { find_end(xml, ct.end, ct.local()).ok_or("unterminated element in <row>")?.end };
        if !ct.closing && ct.local() == "c" {
            let col = attr(ct.text, "r").and_then(col_of).unwrap_or(next_col);
            next_col = col + 1;
            cells.push((col, xml[ct.start..end].to_string()));
        } else if !ct.closing {
            others.push_str(&xml[ct.start..end]);
        }
        i = end;
    }
    Ok((cells, others, close.end))
}

fn cell_style(raw: &str) -> Option<usize> {
    next_tag(raw, 0).and_then(|t| attr(t.text, "s").and_then(|v| v.parse().ok()))
}

fn set_cell_style(raw: &str, xf: usize) -> String {
    let open = next_tag(raw, 0).expect("cell tag");
    format!("{}{}", set_attr(open.text, "s", &xf.to_string()), &raw[open.end..])
}

fn has_value(raw: &str) -> bool {
    let mut i = 0;
    while let Some(t) = next_tag(raw, i) {
        if !t.closing && matches!(t.local(), "v" | "is" | "f") {
            return true;
        }
        i = t.end;
    }
    false
}

fn inline_cell(prefix: &str, col: u32, row: u32, text: &str, style: Option<usize>) -> String {
    let s = style.map(|s| format!(" s=\"{s}\"")).unwrap_or_default();
    format!(
        "<{prefix}c r=\"{}{row}\"{s} t=\"inlineStr\"><{prefix}is><{prefix}t xml:space=\"preserve\">{}</{prefix}t></{prefix}is></{prefix}c>",
        col_letters(col),
        escape(text)
    )
}

fn empty_cell(prefix: &str, col: u32, row: u32, style: Option<usize>) -> String {
    let s = style.map(|s| format!(" s=\"{s}\"")).unwrap_or_default();
    format!("<{prefix}c r=\"{}{row}\"{s}/>", col_letters(col))
}

/// Everything to apply to one worksheet.
struct SheetChanges<'a> {
    statuses: &'a BTreeMap<u32, Status>,
    notes: &'a BTreeMap<u32, String>,
    plan: Option<NotesPlan>,
    /// Columns status rows are filled across (the used range, plus the notes column).
    cols: Option<(u32, u32)>,
}

impl SheetChanges<'_> {
    fn note(&self, row: u32) -> Option<&str> {
        self.notes.get(&row).map(|s| s.as_str()).filter(|s| !s.trim().is_empty())
    }

    /// Rows that must exist in the output (they get a status, a note or the notes header).
    fn wanted_rows(&self) -> std::collections::BTreeSet<u32> {
        let mut rows: std::collections::BTreeSet<u32> = self.statuses.keys().copied().collect();
        rows.extend(self.notes.iter().filter(|(_, t)| !t.trim().is_empty()).map(|(r, _)| *r));
        if let Some(p) = self.plan.filter(|p| p.add_header) {
            rows.insert(p.header_row);
        }
        rows
    }

    /// Rebuilds one row's cells: sets or clears the note cell, then applies or removes the
    /// status fill. Returns the new cells and whether the row style should change.
    fn apply(&self, prefix: &str, row: u32, mut cells: Vec<(u32, String)>, styles: &mut Styles) -> Vec<(u32, String)> {
        if let Some(p) = self.plan {
            let existing = cells.iter().position(|(c, _)| *c == p.col);
            if row == p.header_row {
                if p.add_header && existing.is_none() {
                    cells.push((p.col, inline_cell(prefix, p.col, row, NOTES_HEADER, None)));
                }
            } else {
                match (self.note(row), existing) {
                    (Some(text), Some(i)) => {
                        let style = cell_style(&cells[i].1);
                        cells[i].1 = inline_cell(prefix, p.col, row, text, style);
                    }
                    (Some(text), None) => cells.push((p.col, inline_cell(prefix, p.col, row, text, None))),
                    (None, Some(i)) if has_value(&cells[i].1) => {
                        let style = cell_style(&cells[i].1);
                        cells[i].1 = empty_cell(prefix, p.col, row, style);
                    }
                    _ => {}
                }
            }
        }
        match self.statuses.get(&row) {
            Some(&status) => {
                for (_, raw) in cells.iter_mut() {
                    let xf = styles.variant(cell_style(raw).unwrap_or(0), Some(status));
                    *raw = set_cell_style(raw, xf);
                }
                // Fill gaps so every viewer shows the whole row coloured.
                if let Some((first, last)) = self.cols {
                    let empty_xf = styles.variant(0, Some(status));
                    for c in first..=last {
                        if !cells.iter().any(|(col, _)| *col == c) {
                            cells.push((c, empty_cell(prefix, c, row, Some(empty_xf))));
                        }
                    }
                }
            }
            None => {
                for (_, raw) in cells.iter_mut() {
                    if let Some(base) = cell_style(raw) {
                        let xf = styles.variant(base, None);
                        if xf != base {
                            *raw = set_cell_style(raw, xf);
                        }
                    }
                }
            }
        }
        cells.sort_by_key(|(c, _)| *c);
        cells
    }

    /// The row's open tag with the status row format applied or removed.
    fn row_tag(&self, tag: &str, row: u32, styles: &mut Styles, cells_added_outside_spans: bool) -> String {
        let base = attr(tag, "s").and_then(|v| v.parse::<usize>().ok());
        let mut tag = match (self.statuses.get(&row), base) {
            (Some(&status), base) => {
                let xf = styles.variant(base.unwrap_or(0), Some(status));
                set_attr(&set_attr(tag, "s", &xf.to_string()), "customFormat", "1")
            }
            (None, Some(b)) => {
                let xf = styles.variant(b, None);
                if xf != b { set_attr(tag, "s", &xf.to_string()) } else { tag.to_string() }
            }
            (None, None) => tag.to_string(),
        };
        if cells_added_outside_spans {
            // "spans" is only an optimisation hint; drop it rather than let it be wrong.
            if let Some((_, _, range)) = attrs(&tag).into_iter().find(|(n, _, _)| *n == "spans") {
                let start = tag[..range.start].rfind("spans").unwrap_or(range.start);
                tag = format!("{}{}", tag[..start].trim_end(), &tag[range.end + 1..]);
            }
        }
        tag
    }
}

/// Applies statuses and notes to a worksheet. Rows with nothing to change are copied
/// unchanged.
fn rewrite_sheet(xml: &str, ch: &SheetChanges, styles: &mut Styles) -> Result<String, String> {
    let Some(data) = find_start(xml, 0, "sheetData") else { return Ok(xml.to_string()) };
    let prefix = data.prefix().to_string();
    let mut pending = ch.wanted_rows();
    let first_col = ch.cols.map_or(0, |c| c.0);
    let new_row = |r: u32, styles: &mut Styles| -> String {
        let cells: String = ch.apply(&prefix, r, Vec::new(), styles).into_iter().map(|(_, raw)| raw).collect();
        let tag = ch.row_tag(&format!("<{prefix}row r=\"{r}\">"), r, styles, false);
        format!("{tag}{cells}</{prefix}row>")
    };

    let mut out = String::with_capacity(xml.len() + 4096);
    if data.self_closing {
        if pending.is_empty() {
            return Ok(xml.to_string());
        }
        out.push_str(&xml[..data.start]);
        out.push_str(&format!("<{prefix}sheetData>"));
        for r in std::mem::take(&mut pending) {
            out.push_str(&new_row(r, styles));
        }
        out.push_str(&format!("</{prefix}sheetData>"));
        out.push_str(&xml[data.end..]);
        return Ok(out);
    }

    out.push_str(&xml[..data.end]);
    let mut pos = data.end;
    let mut last_row = 0u32;
    while let Some(t) = next_tag(xml, pos) {
        out.push_str(&xml[pos..t.start]);
        pos = t.end;
        if t.closing && t.local() == "sheetData" {
            for r in std::mem::take(&mut pending) {
                out.push_str(&new_row(r, styles));
            }
            out.push_str(t.text);
            out.push_str(&xml[pos..]);
            return Ok(out);
        }
        if t.closing || t.local() != "row" {
            out.push_str(t.text);
            continue;
        }
        let r = attr(t.text, "r").and_then(|v| v.parse().ok()).unwrap_or(last_row + 1);
        last_row = r;
        // Rows that don't exist yet but sort before this one.
        let before: Vec<u32> = pending.range(..r).copied().collect();
        for nr in before {
            pending.remove(&nr);
            out.push_str(&new_row(nr, styles));
        }
        pending.remove(&r);
        let (cells, others, end) = if t.self_closing { (Vec::new(), String::new(), t.end) } else { parse_row(xml, &t, first_col)? };
        let original = &xml[t.start..end];
        let max_before = cells.iter().map(|(c, _)| *c).max();
        let new_cells = ch.apply(&prefix, r, cells.clone(), styles);
        let max_after = new_cells.iter().map(|(c, _)| *c).max();
        let tag_in = if t.self_closing { format!("{}>", t.text[..t.text.len() - 2].trim_end()) } else { t.text.to_string() };
        let tag = ch.row_tag(&tag_in, r, styles, max_after > max_before);
        let unchanged = new_cells == cells && tag == tag_in;
        if unchanged {
            out.push_str(original);
        } else {
            out.push_str(&tag);
            for (_, raw) in &new_cells {
                out.push_str(raw);
            }
            out.push_str(&others);
            out.push_str(&format!("</{}row>", t.prefix()));
        }
        pos = end;
    }
    Err("worksheet has no closing </sheetData>".into())
}

/// Widens the used range to include the new notes column, and gives that column a
/// readable width (if no existing `<col>` covers it).
fn add_notes_column_layout(xml: &str, plan: &NotesPlan) -> String {
    let mut xml = xml.to_string();
    if let (Some(dim), Some((fr, fc, lr, lc))) = (find_start(&xml, 0, "dimension"), dimension(&xml)) {
        if plan.col > lc {
            let tag = set_attr(dim.text, "ref", &format!("{}{fr}:{}{lr}", col_letters(fc), col_letters(plan.col)));
            xml = format!("{}{tag}{}", &xml[..dim.start], &xml[dim.end..]);
        }
    }
    let n = plan.col + 1; // <col> uses 1-based column numbers
    let col_el = |p: &str| format!("<{p}col min=\"{n}\" max=\"{n}\" width=\"50\" customWidth=\"1\"/>");
    match find_start(&xml, 0, "cols") {
        Some(cols) if !cols.self_closing => {
            let Some(close) = find_end(&xml, cols.end, "cols") else { return xml };
            let mut i = cols.end;
            let mut covered = false;
            let mut all_before = true;
            while let Some(t) = next_tag(&xml, i) {
                if t.start >= close.start {
                    break;
                }
                if !t.closing && t.local() == "col" {
                    let min: u32 = attr(t.text, "min").and_then(|v| v.parse().ok()).unwrap_or(0);
                    let max: u32 = attr(t.text, "max").and_then(|v| v.parse().ok()).unwrap_or(0);
                    covered |= min <= n && n <= max;
                    all_before &= max < n;
                }
                i = t.end;
            }
            if !covered && all_before {
                let el = col_el(cols.prefix());
                xml = format!("{}{el}{}", &xml[..close.start], &xml[close.start..]);
            }
            xml
        }
        Some(_) => xml,
        None => match find_start(&xml, 0, "sheetData") {
            Some(data) => {
                let p = data.prefix().to_string();
                format!("{}<{p}cols>{}</{p}cols>{}", &xml[..data.start], col_el(&p), &xml[data.start..])
            }
            None => xml,
        },
    }
}

// ---------------------------------------------------------------------------
// Workbook (zip) level
// ---------------------------------------------------------------------------

type Archive<'a> = zip::ZipArchive<Cursor<&'a [u8]>>;

fn read_part(zip: &mut Archive, name: &str) -> Result<String, String> {
    let mut f = zip.by_name(name).map_err(|e| format!("missing {name}: {e}"))?;
    let mut s = String::new();
    f.read_to_string(&mut s).map_err(|e| format!("could not read {name}: {e}"))?;
    Ok(s)
}

/// Worksheet name → zip path of its XML part, in workbook order.
fn sheet_parts(zip: &mut Archive) -> Result<Vec<(String, String)>, String> {
    let workbook = read_part(zip, "xl/workbook.xml")?;
    let rels = read_part(zip, "xl/_rels/workbook.xml.rels")?;
    let mut targets = HashMap::new();
    let mut i = 0;
    while let Some(t) = next_tag(&rels, i) {
        if !t.closing && t.local() == "Relationship" {
            if let (Some(id), Some(target)) = (attr(t.text, "Id"), attr(t.text, "Target")) {
                let path = match target.strip_prefix('/') {
                    Some(abs) => abs.to_string(),
                    None => format!("xl/{target}"),
                };
                targets.insert(id.to_string(), path);
            }
        }
        i = t.end;
    }
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(t) = next_tag(&workbook, i) {
        if !t.closing && t.local() == "sheet" {
            let a = attrs(t.text);
            let name = a.iter().find(|(n, _, _)| *n == "name").map(|(_, v, _)| unescape(v));
            // The relationship id attribute is namespaced (usually `r:id`).
            let rid = a.iter().find(|(n, _, _)| n.ends_with(":id")).map(|(_, v, _)| v.to_string());
            if let (Some(name), Some(path)) = (name, rid.and_then(|r| targets.get(&r).cloned())) {
                out.push((name, path));
            }
        }
        i = t.end;
    }
    Ok(out)
}

/// Reads the statuses previously saved into a workbook (rows whose format uses one of the
/// status fill colours).
pub fn read_statuses(bytes: &[u8]) -> Result<Statuses, String> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("not an .xlsx file: {e}"))?;
    let styles = Styles::parse(read_part(&mut zip, "xl/styles.xml")?)?;
    let mut out = Statuses::new();
    if styles.status_fills.is_empty() {
        return Ok(out);
    }
    for (name, path) in sheet_parts(&mut zip)? {
        let xml = read_part(&mut zip, &path)?;
        let mut rows = BTreeMap::new();
        let mut i = 0;
        let mut last = 0u32;
        while let Some(t) = next_tag(&xml, i) {
            if !t.closing && t.local() == "row" {
                let r = attr(t.text, "r").and_then(|v| v.parse().ok()).unwrap_or(last + 1);
                last = r;
                if let Some(status) = attr(t.text, "s").and_then(|v| v.parse().ok()).and_then(|xf| styles.status_of_xf(xf)) {
                    rows.insert(r, status);
                }
            }
            i = t.end;
        }
        if !rows.is_empty() {
            out.insert(name, rows);
        }
    }
    Ok(out)
}

/// Writes `original` with `statuses` and `notes` applied to `out`. The file is written to a
/// temporary name in the same folder and then renamed over `out`, so a failed save never
/// leaves a half-written spreadsheet behind.
pub fn save_changes(original: &[u8], statuses: &Statuses, notes: &Notes, out: &Path) -> Result<(), String> {
    let bytes = apply_changes(original, statuses, notes)?;
    let dir = out.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let tmp = dir.join(format!(".{}.adachecker-saving", out.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()));
    std::fs::write(&tmp, &bytes).map_err(|e| format!("Could not write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, out).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("Could not save {}: {e}", out.display())
    })
}

/// Returns the workbook bytes with `statuses` and `notes` applied. Every worksheet with data
/// gets a "Notes" column (after its last used column) unless it already has one.
pub fn apply_changes(original: &[u8], statuses: &Statuses, notes: &Notes) -> Result<Vec<u8>, String> {
    let mut zip = zip::ZipArchive::new(Cursor::new(original)).map_err(|e| format!("not an .xlsx file: {e}"))?;
    let mut styles = Styles::parse(read_part(&mut zip, "xl/styles.xml")?)?;
    let shared = shared_strings(&mut zip);
    let (no_statuses, no_notes) = (BTreeMap::new(), BTreeMap::new());
    let mut replaced: HashMap<String, String> = HashMap::new();
    for (name, path) in sheet_parts(&mut zip)? {
        let xml = read_part(&mut zip, &path)?;
        let plan = plan_notes(&xml, &shared);
        let cols = dimension(&xml).map(|(_, fc, _, lc)| (fc, plan.map_or(lc, |p| lc.max(p.col))));
        let changes = SheetChanges {
            statuses: statuses.get(&name).unwrap_or(&no_statuses),
            notes: notes.get(&name).unwrap_or(&no_notes),
            plan,
            cols,
        };
        let mut new_xml = rewrite_sheet(&xml, &changes, &mut styles).map_err(|e| format!("{name}: {e}"))?;
        if let Some(p) = plan.filter(|p| p.add_header) {
            new_xml = add_notes_column_layout(&new_xml, &p);
        }
        if new_xml != xml {
            replaced.insert(path, new_xml);
        }
    }
    let styles_xml = styles.finish();
    replaced.insert("xl/styles.xml".into(), styles_xml);

    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::with_capacity(original.len() + 4096)));
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for i in 0..zip.len() {
        let file = zip.by_index_raw(i).map_err(|e| e.to_string())?;
        let name = file.name().map_err(|e| e.to_string())?.into_owned();
        match replaced.remove(&name) {
            Some(content) => {
                drop(file);
                writer.start_file(name.as_str(), options).map_err(|e| e.to_string())?;
                writer.write_all(content.as_bytes()).map_err(|e| e.to_string())?;
            }
            None => writer.raw_copy_file(file).map_err(|e| e.to_string())?,
        }
    }
    Ok(writer.finish().map_err(|e| e.to_string())?.into_inner())
}

/// `apply_changes` without notes (used by tests).
#[cfg(test)]
fn apply_statuses(original: &[u8], statuses: &Statuses) -> Result<Vec<u8>, String> {
    apply_changes(original, statuses, &Notes::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use calamine::{Reader, Xlsx};
    use rust_xlsxwriter::{Color, Format, Workbook};

    fn sample() -> Vec<u8> {
        let mut book = Workbook::new();
        let bold = Format::new().set_bold();
        let blue = Format::new().set_background_color(Color::RGB(0x9DC3E6));
        let s = book.add_worksheet().set_name("Issues & notes").unwrap();
        for (c, h) in ["Guidelines", "Priority", "URL"].iter().enumerate() {
            s.write_string_with_format(0, c as u16, *h, &bold).unwrap();
        }
        for r in 1..=5u32 {
            s.write_string(r, 0, format!("Issue {r}")).unwrap();
            s.write_number(r, 1, r as f64).unwrap();
        }
        // Row 4 (Excel numbering) has a user's own blue fill on one cell.
        s.write_string_with_format(3, 2, "https://example.com/blue", &blue).unwrap();
        // Row 8 has no cells at all.
        s.set_column_width(2, 40).unwrap();
        let other = book.add_worksheet().set_name("Other").unwrap();
        other.write_string(0, 0, "untouched").unwrap();
        book.save_to_buffer().unwrap()
    }

    fn statuses(rows: &[(u32, Status)]) -> Statuses {
        let mut s = Statuses::new();
        s.insert("Issues & notes".into(), rows.iter().copied().collect());
        s
    }

    /// Cell values of a worksheet, without the Notes column (which saving adds).
    fn values(bytes: &[u8], sheet: &str) -> Vec<Vec<String>> {
        strip_notes(raw_values(bytes, sheet))
    }

    fn raw_values(bytes: &[u8], sheet: &str) -> Vec<Vec<String>> {
        let mut x: Xlsx<_> = Xlsx::new(Cursor::new(bytes.to_vec())).unwrap();
        x.worksheet_range(sheet).unwrap().rows().map(|r| r.iter().map(|c| c.to_string()).collect()).collect()
    }

    fn strip_notes(mut rows: Vec<Vec<String>>) -> Vec<Vec<String>> {
        if let Some(c) = rows.first().and_then(|h| h.iter().position(|v| v == NOTES_HEADER)) {
            for r in rows.iter_mut() {
                if c < r.len() {
                    r.remove(c);
                }
            }
        }
        rows
    }

    fn notes(rows: &[(u32, &str)]) -> Notes {
        let mut n = Notes::new();
        n.insert("Issues & notes".into(), rows.iter().map(|(r, t)| (*r, t.to_string())).collect());
        n
    }

    #[test]
    fn fixed_path_naming() {
        assert_eq!(fixed_path(Path::new("/d/ada.xlsx")), Path::new("/d/ada-fixed.xlsx"));
        assert_eq!(fixed_path(Path::new("/d/ada-fixed.xlsx")), Path::new("/d/ada-fixed.xlsx"));
        assert_eq!(fixed_path(Path::new("/d/my.report.xlsm")), Path::new("/d/my.report-fixed.xlsm"));
        assert!(is_editable(Path::new("a.XLSX")) && is_editable(Path::new("a.xlsm")));
        assert!(!is_editable(Path::new("a.xls")) && !is_editable(Path::new("a.ods")));
    }

    #[test]
    fn attribute_editing() {
        assert_eq!(set_attr(r#"<c r="A1" t="s">"#, "s", "5"), r#"<c r="A1" t="s" s="5">"#);
        assert_eq!(set_attr(r#"<c r="A1" s="2" t="s"/>"#, "s", "7"), r#"<c r="A1" s="7" t="s"/>"#);
        assert_eq!(attr(r#"<row r="3" spans="1:9" s="4">"#, "s"), Some("4"));
        assert_eq!(attr(r#"<c r="B2" t="s">"#, "s"), None, "t=\"s\" is not attribute s");
    }

    #[test]
    fn saves_statuses_and_keeps_everything_else() {
        let original = sample();
        let out = apply_statuses(&original, &statuses(&[(2, Status::Fixed), (3, Status::OnHold), (4, Status::NotApplicable), (5, Status::Broken), (8, Status::Fixed)])).unwrap();

        // Data is unchanged, in both sheets.
        assert_eq!(values(&out, "Issues & notes"), values(&original, "Issues & notes"));
        assert_eq!(values(&out, "Other"), [["untouched"]]);

        // Statuses read back, including the row that had no cells.
        let back = read_statuses(&out).unwrap();
        let rows = &back["Issues & notes"];
        assert_eq!(rows.get(&2), Some(&Status::Fixed));
        assert_eq!(rows.get(&3), Some(&Status::OnHold));
        assert_eq!(rows.get(&4), Some(&Status::NotApplicable));
        assert_eq!(rows.get(&5), Some(&Status::Broken));
        assert_eq!(rows.get(&8), Some(&Status::Fixed));
        assert_eq!(rows.len(), 5);
        assert!(!back.contains_key("Other"));

        // Each status cell uses a format with the right fill; the header's bold font is untouched.
        let mut zip = zip::ZipArchive::new(Cursor::new(&out[..])).unwrap();
        let styles = Styles::parse(read_part(&mut zip, "xl/styles.xml").unwrap()).unwrap();
        let sheet = read_part(&mut zip, "xl/worksheets/sheet1.xml").unwrap();
        let cell_xf = |cell: &str| -> usize {
            let at = sheet.find(&format!("r=\"{cell}\"")).unwrap();
            let start = sheet[..at].rfind('<').unwrap();
            let tag = next_tag(&sheet, start).unwrap();
            attr(tag.text, "s").and_then(|v| v.parse().ok()).unwrap_or(0)
        };
        assert_eq!(styles.status_of_xf(cell_xf("A2")), Some(Status::Fixed));
        assert_eq!(styles.status_of_xf(cell_xf("B3")), Some(Status::OnHold));
        assert_eq!(styles.status_of_xf(cell_xf("C4")), Some(Status::NotApplicable), "user's blue fill replaced by the status fill");
        assert_eq!(styles.status_of_xf(cell_xf("A6")), None);
        let header_xf = &styles.xfs[cell_xf("A1")];
        assert!(attr(header_xf, "fontId").unwrap() != "0", "bold header keeps its font");
        // Column widths (<cols>) survive.
        assert!(sheet.contains("<cols>"));
    }

    #[test]
    fn status_rows_fill_every_used_column() {
        // Row 2 has cells in A and B only; the sheet's used range is A:C.
        let out = apply_statuses(&sample(), &statuses(&[(2, Status::Fixed), (8, Status::OnHold)])).unwrap();
        let mut zip = zip::ZipArchive::new(Cursor::new(&out[..])).unwrap();
        let styles = Styles::parse(read_part(&mut zip, "xl/styles.xml").unwrap()).unwrap();
        let sheet = read_part(&mut zip, "xl/worksheets/sheet1.xml").unwrap();
        for (cell, status) in [("A2", Status::Fixed), ("B2", Status::Fixed), ("C2", Status::Fixed), ("A8", Status::OnHold), ("C8", Status::OnHold)] {
            let at = sheet.find(&format!("r=\"{cell}\"")).unwrap_or_else(|| panic!("{cell} missing"));
            let tag = next_tag(&sheet, sheet[..at].rfind('<').unwrap()).unwrap();
            let xf: usize = attr(tag.text, "s").unwrap().parse().unwrap();
            assert_eq!(styles.status_of_xf(xf), Some(status), "{cell}");
        }
        // Cells stay in column order within the row.
        let row2 = &sheet[sheet.find("<row r=\"2\"").unwrap()..];
        let row2 = &row2[..row2.find("</row>").unwrap()];
        let (a, b, c) = (row2.find("\"A2\"").unwrap(), row2.find("\"B2\"").unwrap(), row2.find("\"C2\"").unwrap());
        assert!(a < b && b < c);
        assert_eq!(col_of("AA10"), Some(26));
        assert_eq!(col_letters(26), "AA");
    }

    #[test]
    fn writes_notes_in_a_notes_column() {
        let out = apply_changes(
            &sample(),
            &statuses(&[(2, Status::Fixed)]),
            &notes(&[(2, "Checked by DK"), (4, "a < b & \"c\""), (5, "line one\nline two"), (3, "   ")]),
        )
        .unwrap();
        let v = raw_values(&out, "Issues & notes");
        assert_eq!(v[0], ["Guidelines", "Priority", "URL", "Notes"], "header added after the last column");
        assert_eq!(v[1][3], "Checked by DK");
        assert_eq!(v[2][3], "", "a blank note is not written");
        assert_eq!(v[3][3], "a < b & \"c\"", "special characters escaped and read back");
        assert_eq!(v[4][3], "line one\nline two");
        assert_eq!(values(&out, "Issues & notes"), values(&sample(), "Issues & notes"), "other data unchanged");
        assert_eq!(raw_values(&out, "Other"), [["untouched", "Notes"]], "every sheet gets the column");

        let mut zip = zip::ZipArchive::new(Cursor::new(&out[..])).unwrap();
        let styles = Styles::parse(read_part(&mut zip, "xl/styles.xml").unwrap()).unwrap();
        let sheet = read_part(&mut zip, "xl/worksheets/sheet1.xml").unwrap();
        let at = sheet.find("r=\"D2\"").unwrap();
        let tag = next_tag(&sheet, sheet[..at].rfind('<').unwrap()).unwrap();
        assert_eq!(styles.status_of_xf(attr(tag.text, "s").unwrap().parse().unwrap()), Some(Status::Fixed), "status colour covers the note");
        assert!(sheet.contains("<dimension ref=\"A1:D"), "used range includes the notes column: {}", &sheet[..300.min(sheet.len())]);
        assert!(sheet.contains("<col min=\"4\" max=\"4\" width=\"50\""), "notes column gets a readable width");
        assert!(read_statuses(&out).unwrap()["Issues & notes"].get(&2) == Some(&Status::Fixed));
    }

    #[test]
    fn resaving_updates_and_clears_notes_without_a_second_column() {
        let first = apply_changes(&sample(), &Statuses::new(), &notes(&[(2, "first"), (4, "to be cleared")])).unwrap();
        let second = apply_changes(&first, &Statuses::new(), &notes(&[(2, "edited"), (6, "new")])).unwrap();
        let v = raw_values(&second, "Issues & notes");
        assert_eq!(v[0].iter().filter(|h| *h == "Notes").count(), 1, "existing Notes column reused");
        assert_eq!(v[1][3], "edited");
        assert_eq!(v[3][3], "", "cleared");
        assert_eq!(v[5][3], "new");
    }

    #[test]
    fn resaving_from_a_fixed_file_updates_and_clears() {
        let first = apply_statuses(&sample(), &statuses(&[(2, Status::Fixed), (4, Status::OnHold)])).unwrap();
        // Reopen the -fixed file: change row 2, clear row 4, add row 5.
        let second = apply_statuses(&first, &statuses(&[(2, Status::NotApplicable), (5, Status::Fixed)])).unwrap();
        let back = read_statuses(&second).unwrap();
        let rows = &back["Issues & notes"];
        assert_eq!(rows.get(&2), Some(&Status::NotApplicable));
        assert_eq!(rows.get(&4), None, "cleared status is removed");
        assert_eq!(rows.get(&5), Some(&Status::Fixed));
        assert_eq!(values(&second, "Issues & notes"), values(&sample(), "Issues & notes"));
        // Existing status fills are reused rather than added again.
        let mut zip = zip::ZipArchive::new(Cursor::new(&second[..])).unwrap();
        let styles = Styles::parse(read_part(&mut zip, "xl/styles.xml").unwrap()).unwrap();
        let fixed_fills = styles.status_fills.values().filter(|s| **s == Status::Fixed).count();
        assert_eq!(fixed_fills, 1);
    }

    #[test]
    fn saving_with_no_statuses_round_trips() {
        let original = sample();
        let out = apply_statuses(&original, &Statuses::new()).unwrap();
        assert!(read_statuses(&out).unwrap().is_empty());
        assert_eq!(values(&out, "Issues & notes"), values(&original, "Issues & notes"));
    }

    /// `XLSX_ORIGINAL=a.xlsx XLSX_SAVED=a-fixed.xlsx cargo test compare_saved_file -- --ignored --nocapture`
    #[test]
    #[ignore = "needs XLSX_ORIGINAL and XLSX_SAVED"]
    fn compare_saved_file() {
        let a = std::fs::read(std::env::var("XLSX_ORIGINAL").unwrap()).unwrap();
        let b = std::fs::read(std::env::var("XLSX_SAVED").unwrap()).unwrap();
        let mut xa: Xlsx<_> = Xlsx::new(Cursor::new(a.clone())).unwrap();
        let mut xb: Xlsx<_> = Xlsx::new(Cursor::new(b.clone())).unwrap();
        assert_eq!(xa.sheet_names(), xb.sheet_names());
        for name in xa.sheet_names() {
            let ra = strip_notes(xa.worksheet_range(&name).unwrap().rows().map(|r| r.iter().map(|c| c.to_string()).collect()).collect());
            let rb = strip_notes(xb.worksheet_range(&name).unwrap().rows().map(|r| r.iter().map(|c| c.to_string()).collect()).collect());
            assert_eq!(ra, rb, "sheet {name} data differs");
            println!("sheet {name:?}: {} rows identical", ra.len());
        }
        let mut za = zip::ZipArchive::new(Cursor::new(&a[..])).unwrap();
        let mut zb = zip::ZipArchive::new(Cursor::new(&b[..])).unwrap();
        let names = |z: &mut Archive| (0..z.len()).map(|i| z.by_index_raw(i).unwrap().name().unwrap().into_owned()).collect::<Vec<_>>();
        assert_eq!(names(&mut za), names(&mut zb), "same parts in the same order");
        let mut changed = Vec::new();
        for n in names(&mut za) {
            if read_part(&mut za, &n).ok() != read_part(&mut zb, &n).ok() {
                changed.push(n);
            }
        }
        println!("changed parts: {changed:?}");
        println!("statuses in saved file: {:?}", read_statuses(&b).unwrap());
    }

    /// `XLSX_DEMO_IN=a.xlsx XLSX_DEMO_OUT=b.xlsx cargo test demo_all_statuses -- --ignored`:
    /// marks rows 2, 3 and 4 of the first worksheet Fixed, On-Hold and N/A.
    #[test]
    #[ignore = "needs XLSX_DEMO_IN and XLSX_DEMO_OUT"]
    fn demo_all_statuses() {
        let input = std::fs::read(std::env::var("XLSX_DEMO_IN").unwrap()).unwrap();
        let mut zip = zip::ZipArchive::new(Cursor::new(&input[..])).unwrap();
        let first = sheet_parts(&mut zip).unwrap().remove(0).0;
        let mut st = Statuses::new();
        st.insert(first, [(2, Status::Fixed), (3, Status::OnHold), (4, Status::NotApplicable)].into_iter().collect());
        save_changes(&input, &st, &Notes::new(), Path::new(&std::env::var("XLSX_DEMO_OUT").unwrap())).unwrap();
    }

    #[test]
    fn writes_atomically_to_disk() {
        let dir = std::env::temp_dir().join(format!("adachecker-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("report-fixed.xlsx");
        save_changes(&sample(), &statuses(&[(3, Status::Fixed)]), &Notes::new(), &out).unwrap();
        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(read_statuses(&bytes).unwrap()["Issues & notes"].get(&3), Some(&Status::Fixed));
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().contains("saving")).collect();
        assert!(leftovers.is_empty(), "temporary file removed");
        std::fs::remove_dir_all(&dir).ok();
    }
}
