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
}

impl Status {
    pub const ALL: [Status; 3] = [Status::Fixed, Status::OnHold, Status::NotApplicable];

    pub fn label(self) -> &'static str {
        match self {
            Status::Fixed => "Fixed",
            Status::OnHold => "On-Hold",
            Status::NotApplicable => "N/A",
        }
    }

    /// Fill colour written to the spreadsheet (ARGB): Excel's "Good" green, "Neutral"
    /// yellow, and a light grey — all readable with black text.
    pub fn argb(self) -> &'static str {
        match self {
            Status::Fixed => "FFC6EFCE",
            Status::OnHold => "FFFFEB9C",
            Status::NotApplicable => "FFD9D9D9",
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

/// The sheet's used column range from `<dimension ref="A1:I29"/>`.
fn used_columns(xml: &str) -> Option<(u32, u32)> {
    let dim = find_start(xml, 0, "dimension")?;
    let r = attr(dim.text, "ref")?;
    let (a, b) = r.split_once(':').unwrap_or((r, r));
    Some((col_of(a)?, col_of(b)?))
}

/// The cells of a status row: existing cells restyled with the status fill, plus empty
/// filled cells for any gaps across the sheet's used columns, in column order. (Excel
/// colours missing cells from the row format, but other viewers only colour real cells.)
fn status_row_cells(
    prefix: &str,
    row: u32,
    existing: Vec<(u32, String)>,
    cols: Option<(u32, u32)>,
    status: Status,
    styles: &mut Styles,
) -> String {
    let mut cells = existing;
    if let Some((first, last)) = cols {
        let empty_xf = styles.variant(0, Some(status));
        for c in first..=last {
            if !cells.iter().any(|(col, _)| *col == c) {
                cells.push((c, format!("<{prefix}c r=\"{}{row}\" s=\"{empty_xf}\"/>", col_letters(c))));
            }
        }
    }
    cells.sort_by_key(|(c, _)| *c);
    cells.into_iter().map(|(_, raw)| raw).collect()
}

/// Applies `rows` (row number → status) to a worksheet: status rows get the matching fill on
/// the row and on each cell; rows without a status lose any status fill from an earlier save.
fn rewrite_sheet(xml: &str, rows: &BTreeMap<u32, Status>, styles: &mut Styles) -> Result<String, String> {
    let Some(data) = find_start(xml, 0, "sheetData") else { return Ok(xml.to_string()) };
    let prefix = data.prefix().to_string();
    let cols = used_columns(xml);
    let mut out = String::with_capacity(xml.len() + rows.len() * 200);
    let mut pending: BTreeMap<u32, Status> = rows.clone();
    // Status rows that don't exist in the sheet yet.
    let emit_pending_before = |out: &mut String, limit: Option<u32>, styles: &mut Styles, pending: &mut BTreeMap<u32, Status>| {
        let keys: Vec<u32> = pending.keys().copied().filter(|r| limit.is_none_or(|l| *r < l)).collect();
        for r in keys {
            let status = pending.remove(&r).expect("pending row");
            let xf = styles.variant(0, Some(status));
            let cells = status_row_cells(&prefix, r, Vec::new(), cols, status, styles);
            out.push_str(&format!("<{prefix}row r=\"{r}\" s=\"{xf}\" customFormat=\"1\">{cells}</{prefix}row>"));
        }
    };

    if data.self_closing {
        if rows.is_empty() {
            return Ok(xml.to_string());
        }
        out.push_str(&xml[..data.start]);
        out.push_str(&format!("<{prefix}sheetData>"));
        emit_pending_before(&mut out, None, styles, &mut pending);
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
            emit_pending_before(&mut out, None, styles, &mut pending);
            out.push_str(t.text);
            out.push_str(&xml[pos..]);
            return Ok(out);
        }
        if !t.closing && t.local() == "row" {
            let r = attr(t.text, "r").and_then(|v| v.parse().ok()).unwrap_or(last_row + 1);
            last_row = r;
            emit_pending_before(&mut out, Some(r), styles, &mut pending);
            let base = attr(t.text, "s").and_then(|v| v.parse::<usize>().ok());
            let Some(status) = pending.remove(&r) else {
                // Not a status row: drop any status fill left from an earlier save.
                match base.map(|b| (b, styles.variant(b, None))) {
                    Some((b, xf)) if xf != b => out.push_str(&set_attr(t.text, "s", &xf.to_string())),
                    _ => out.push_str(t.text),
                }
                continue;
            };
            // A status row: rebuild it with every cell filled.
            let xf = styles.variant(base.unwrap_or(0), Some(status));
            let tag = set_attr(&set_attr(t.text, "s", &xf.to_string()), "customFormat", "1");
            let open_tag = if t.self_closing { format!("{}>", tag[..tag.len() - 2].trim_end()) } else { tag };
            let mut existing = Vec::new();
            let mut others = String::new();
            if !t.self_closing {
                let close = find_end(xml, t.end, "row").ok_or("unterminated <row>")?;
                let mut i = t.end;
                let mut next_col = cols.map_or(0, |c| c.0);
                while let Some(ct) = next_tag(xml, i) {
                    if ct.start >= close.start {
                        break;
                    }
                    let end = if ct.self_closing || ct.closing { ct.end } else { find_end(xml, ct.end, ct.local()).ok_or("unterminated element in <row>")?.end };
                    if !ct.closing && ct.local() == "c" {
                        let col = attr(ct.text, "r").and_then(col_of).unwrap_or(next_col);
                        next_col = col + 1;
                        let cell_base = attr(ct.text, "s").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
                        let cell_xf = styles.variant(cell_base, Some(status));
                        let new_tag = set_attr(ct.text, "s", &cell_xf.to_string());
                        existing.push((col, format!("{new_tag}{}", &xml[ct.end..end])));
                    } else if !ct.closing {
                        others.push_str(&xml[ct.start..end]);
                    }
                    i = end;
                }
                pos = close.end;
            }
            out.push_str(&open_tag);
            out.push_str(&status_row_cells(&prefix, r, existing, cols, status, styles));
            out.push_str(&others);
            out.push_str(&format!("</{}row>", t.prefix()));
            continue;
        }
        if !t.closing && t.local() == "c" {
            // Cell in a non-status row: drop any status fill left from an earlier save.
            match attr(t.text, "s").and_then(|v| v.parse::<usize>().ok()).map(|b| (b, styles.variant(b, None))) {
                Some((b, xf)) if xf != b => out.push_str(&set_attr(t.text, "s", &xf.to_string())),
                _ => out.push_str(t.text),
            }
            continue;
        }
        out.push_str(t.text);
    }
    Err("worksheet has no closing </sheetData>".into())
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

/// Writes `original` with `statuses` applied to `out`. The file is written to a temporary
/// name in the same folder and then renamed over `out`, so a failed save never leaves a
/// half-written spreadsheet behind.
pub fn save_with_statuses(original: &[u8], statuses: &Statuses, out: &Path) -> Result<(), String> {
    let bytes = apply_statuses(original, statuses)?;
    let dir = out.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let tmp = dir.join(format!(".{}.adachecker-saving", out.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()));
    std::fs::write(&tmp, &bytes).map_err(|e| format!("Could not write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, out).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("Could not save {}: {e}", out.display())
    })
}

/// Returns the workbook bytes with `statuses` applied.
pub fn apply_statuses(original: &[u8], statuses: &Statuses) -> Result<Vec<u8>, String> {
    let mut zip = zip::ZipArchive::new(Cursor::new(original)).map_err(|e| format!("not an .xlsx file: {e}"))?;
    let mut styles = Styles::parse(read_part(&mut zip, "xl/styles.xml")?)?;
    let empty = BTreeMap::new();
    let mut replaced: HashMap<String, String> = HashMap::new();
    for (name, path) in sheet_parts(&mut zip)? {
        let xml = read_part(&mut zip, &path)?;
        let rows = statuses.get(&name).unwrap_or(&empty);
        let new_xml = rewrite_sheet(&xml, rows, &mut styles).map_err(|e| format!("{name}: {e}"))?;
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

    fn values(bytes: &[u8], sheet: &str) -> Vec<Vec<String>> {
        let mut x: Xlsx<_> = Xlsx::new(Cursor::new(bytes.to_vec())).unwrap();
        x.worksheet_range(sheet).unwrap().rows().map(|r| r.iter().map(|c| c.to_string()).collect()).collect()
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
        let out = apply_statuses(&original, &statuses(&[(2, Status::Fixed), (3, Status::OnHold), (4, Status::NotApplicable), (8, Status::Fixed)])).unwrap();

        // Data is unchanged, in both sheets.
        assert_eq!(values(&out, "Issues & notes"), values(&original, "Issues & notes"));
        assert_eq!(values(&out, "Other"), [["untouched"]]);

        // Statuses read back, including the row that had no cells.
        let back = read_statuses(&out).unwrap();
        let rows = &back["Issues & notes"];
        assert_eq!(rows.get(&2), Some(&Status::Fixed));
        assert_eq!(rows.get(&3), Some(&Status::OnHold));
        assert_eq!(rows.get(&4), Some(&Status::NotApplicable));
        assert_eq!(rows.get(&8), Some(&Status::Fixed));
        assert_eq!(rows.len(), 4);
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
            let ra: Vec<Vec<String>> = xa.worksheet_range(&name).unwrap().rows().map(|r| r.iter().map(|c| c.to_string()).collect()).collect();
            let rb: Vec<Vec<String>> = xb.worksheet_range(&name).unwrap().rows().map(|r| r.iter().map(|c| c.to_string()).collect()).collect();
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
        save_with_statuses(&input, &st, Path::new(&std::env::var("XLSX_DEMO_OUT").unwrap())).unwrap();
    }

    #[test]
    fn writes_atomically_to_disk() {
        let dir = std::env::temp_dir().join(format!("adachecker-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("report-fixed.xlsx");
        save_with_statuses(&sample(), &statuses(&[(3, Status::Fixed)]), &out).unwrap();
        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(read_statuses(&bytes).unwrap()["Issues & notes"].get(&3), Some(&Status::Fixed));
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().contains("saving")).collect();
        assert!(leftovers.is_empty(), "temporary file removed");
        std::fs::remove_dir_all(&dir).ok();
    }
}
