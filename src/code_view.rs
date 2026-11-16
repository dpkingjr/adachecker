//! Virtualized, syntax-coloured HTML source view with finding highlighting.

use crate::checks::{Finding, Severity};
use crate::source::{self, Class, Row, SourceDoc};
use std::sync::Arc;
use eframe::egui::{self, text::LayoutJob, Color32, FontId, RichText, TextFormat};
use std::ops::Range;

/// A finding's start-tag location, shown as a clickable mark in the source.
#[derive(Clone, Debug)]
pub struct Mark {
    pub range: Range<usize>,
    pub severity: Severity,
    /// (rule index, finding index) in the report.
    pub key: (usize, usize),
    pub label: String,
}

#[derive(Default)]
pub struct CodeView {
    /// All findings with a source location, sorted by start offset.
    marks: Vec<Mark>,
    hide_marks: bool,
    highlight: Option<(Range<usize>, Severity)>,
    /// Message shown above the code when the selected finding has no source location.
    note: Option<String>,
    /// Byte offset to scroll to on the next frame.
    scroll_to: Option<usize>,
    /// Display rows wrapped to the panel width: (document, chars per row, rows).
    layout: Option<(Arc<SourceDoc>, usize, Vec<Row>)>,
}

struct Colors {
    text: Color32,
    punct: Color32,
    tag: Color32,
    attr: Color32,
    value: Color32,
    comment: Color32,
    raw: Color32,
    gutter: Color32,
    error_bg: Color32,
    warning_bg: Color32,
    error_faint: Color32,
    warning_faint: Color32,
    marker: Color32,
    warning_fg: Color32,
}

fn colors(dark: bool) -> Colors {
    // All foreground colours keep ≥ 4.5:1 contrast on the panel and on the highlight backgrounds.
    if dark {
        Colors {
            text: Color32::from_rgb(225, 228, 232),
            punct: Color32::from_rgb(160, 168, 178),
            tag: Color32::from_rgb(121, 184, 255),
            attr: Color32::from_rgb(200, 170, 255),
            value: Color32::from_rgb(158, 220, 160),
            comment: Color32::from_rgb(150, 150, 150),
            raw: Color32::from_rgb(200, 200, 190),
            gutter: Color32::from_rgb(130, 136, 145),
            error_bg: Color32::from_rgb(105, 25, 30),
            warning_bg: Color32::from_rgb(92, 64, 0),
            error_faint: Color32::from_rgb(52, 26, 30),
            warning_faint: Color32::from_rgb(48, 40, 14),
            marker: Color32::from_rgb(255, 138, 128),
            warning_fg: Color32::from_rgb(255, 204, 102),
        }
    } else {
        Colors {
            text: Color32::from_rgb(36, 41, 46),
            punct: Color32::from_rgb(88, 96, 105),
            tag: Color32::from_rgb(0, 82, 170),
            attr: Color32::from_rgb(111, 66, 193),
            value: Color32::from_rgb(20, 110, 40),
            comment: Color32::from_rgb(106, 115, 125),
            raw: Color32::from_rgb(70, 70, 60),
            gutter: Color32::from_rgb(110, 118, 128),
            error_bg: Color32::from_rgb(255, 215, 215),
            warning_bg: Color32::from_rgb(255, 236, 179),
            error_faint: Color32::from_rgb(255, 240, 240),
            warning_faint: Color32::from_rgb(255, 248, 225),
            marker: Color32::from_rgb(176, 0, 32),
            warning_fg: Color32::from_rgb(138, 76, 0),
        }
    }
}

impl CodeView {
    /// Sets the findings to mark in the source (all of them, not just the selection).
    pub fn set_marks(&mut self, mut marks: Vec<Mark>) {
        marks.sort_by_key(|m| (m.range.start, m.severity != Severity::Error));
        self.marks = marks;
    }

    pub fn set_marks_visible(&mut self, visible: bool) {
        self.hide_marks = !visible;
    }

    /// Marks overlapping `row`. Start tags never nest, so overlapping marks are contiguous.
    fn row_marks(&self, row: &Row) -> &[Mark] {
        if self.hide_marks {
            return &[];
        }
        let end = self.marks.partition_point(|m| m.range.start < row.range.end.max(row.range.start + 1));
        let mut start = end;
        while start > 0 && self.marks[start - 1].range.end > row.range.start {
            start -= 1;
        }
        &self.marks[start..end]
    }

    pub fn clear(&mut self) {
        self.highlight = None;
        self.note = None;
        self.scroll_to = None;
    }

    /// Selects `finding`'s source location (if known) and scrolls to it.
    pub fn select(&mut self, finding: &Finding) {
        match finding.locator.as_ref() {
            None => {
                self.highlight = None;
                self.note = Some("Page-level issue: it isn't tied to a single element.".into());
            }
            Some(loc) => match loc.source.clone() {
                Some(range) => {
                    self.scroll_to = Some(range.start);
                    self.highlight = Some((range, finding.severity));
                    self.note = None;
                }
                None => {
                    self.highlight = None;
                    self.note = Some(format!(
                        "No source tag for this <{}>: it was created implicitly by the HTML parser.",
                        loc.tag
                    ));
                }
            },
        }
    }

    /// Draws the source. Returns the finding whose mark the user clicked, if any.
    pub fn show(&mut self, ui: &mut egui::Ui, doc: &Arc<SourceDoc>) -> Option<(usize, usize)> {
        let c = colors(ui.visuals().dark_mode);
        let font = FontId::monospace(12.5);
        let digits = doc.line_count().max(1).to_string().len();

        // Wrap rows to the panel width (gutter = digits + space + 2-char marker; 12 px frame margins).
        let char_w = ui.fonts_mut(|f| f.glyph_width(&font, 'M')).max(1.0);
        let wrap = (((ui.available_width() - 12.0 - 16.0) / char_w) as usize).saturating_sub(digits + 3).max(20);
        let stale = self.layout.as_ref().is_none_or(|(d, w, _)| !Arc::ptr_eq(d, doc) || *w != wrap);
        if stale {
            self.layout = Some((doc.clone(), wrap, doc.wrap_rows(wrap)));
        }
        let rows = &self.layout.as_ref().expect("layout computed").2;

        // Header: where the highlight is, or why there isn't one.
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{} lines · {:.1} KB", doc.line_count(), doc.text.len() as f64 / 1024.0))
                    .small()
                    .color(c.gutter),
            );
            if let Some((range, _)) = &self.highlight {
                ui.label(RichText::new(format!("· selected: line {}", doc.line_of(range.start) + 1)).small().color(c.gutter));
                if ui.small_button("Jump to selection").clicked() {
                    self.scroll_to = Some(range.start);
                }
            }
        });
        if let Some(note) = &self.note {
            ui.label(RichText::new(note).italics().color(c.marker));
        }
        ui.add_space(2.0);

        let row_height = ui.fonts_mut(|f| f.row_height(&font));
        let mut area = egui::ScrollArea::both().id_salt("html-source").auto_shrink([false, false]);
        if let Some(offset) = self.scroll_to.take() {
            let row = source::row_of(rows, offset);
            let offset = (row as f32 * row_height - ui.available_height() / 3.0).max(0.0);
            area = area.vertical_scroll_offset(offset).horizontal_scroll_offset(0.0);
        }

        let mut clicked = None;
        egui::Frame::new().fill(ui.visuals().extreme_bg_color).inner_margin(6).corner_radius(4.0).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            area.show_rows(ui, row_height, rows.len(), |ui, visible| {
                for i in visible {
                    let row = &rows[i];
                    let selected = self
                        .highlight
                        .as_ref()
                        .is_some_and(|(r, _)| r.start < row.range.end.max(row.range.start + 1) && r.end > row.range.start);
                    let marks = self.row_marks(row);
                    let job = row_job(doc, row, &c, &font, digits, self.highlight.as_ref(), selected, marks);
                    let label = egui::Label::new(job).selectable(false).extend();
                    if marks.is_empty() {
                        ui.add(label);
                        continue;
                    }
                    let resp = ui
                        .add(label.sense(egui::Sense::click()))
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text(mark_tooltip(marks));
                    if resp.clicked() {
                        // Resolve the click to the tag under the pointer (several can share a row).
                        let col = resp
                            .interact_pointer_pos()
                            .map(|p| ((p.x - resp.rect.min.x) / char_w) as usize)
                            .unwrap_or(0)
                            .saturating_sub(digits + 3);
                        let offset = row.range.start + byte_at_column(&doc.text[row.range.clone()], col);
                        let m = marks.iter().find(|m| m.range.contains(&offset)).unwrap_or(&marks[0]);
                        clicked = Some(m.key);
                    }
                }
            });
        });
        clicked
    }
}

fn mark_tooltip(marks: &[Mark]) -> String {
    let mut labels: Vec<&str> = Vec::new();
    for m in marks {
        if !labels.contains(&m.label.as_str()) {
            labels.push(&m.label);
        }
    }
    format!("{}\n\nClick to show this element in the page preview.", labels.join("\n"))
}

/// Byte offset within `text` of display column `col` (tabs render as 4 columns).
fn byte_at_column(text: &str, col: usize) -> usize {
    let mut c = 0;
    for (i, ch) in text.char_indices() {
        let w = if ch == '\t' { 4 } else { 1 };
        if col < c + w {
            return i;
        }
        c += w;
    }
    text.len().saturating_sub(1)
}

fn class_color(class: Class, c: &Colors) -> Color32 {
    match class {
        Class::Text => c.text,
        Class::Punct => c.punct,
        Class::TagName => c.tag,
        Class::AttrName => c.attr,
        Class::AttrValue => c.value,
        Class::Comment | Class::Doctype => c.comment,
        Class::RawText => c.raw,
    }
}

#[allow(clippy::too_many_arguments)]
fn row_job(
    doc: &SourceDoc,
    row: &Row,
    c: &Colors,
    font: &FontId,
    digits: usize,
    highlight: Option<&(Range<usize>, Severity)>,
    row_selected: bool,
    marks: &[Mark],
) -> LayoutJob {
    let mut job = LayoutJob::default();
    let gutter = if row.first { format!("{:>digits$} ", row.line + 1) } else { format!("{:>digits$} ", "↪") };
    job.append(&gutter, 0.0, TextFormat::simple(font.clone(), c.gutter));
    let row_has_error = marks.iter().any(|m| m.severity == Severity::Error);
    let (marker, marker_color) = if row_selected {
        ("▶ ", c.marker)
    } else if !marks.is_empty() {
        ("● ", if row_has_error { c.marker } else { c.warning_fg })
    } else {
        ("  ", c.gutter)
    };
    job.append(marker, 0.0, TextFormat::simple(font.clone(), marker_color));

    let in_hl = |i: usize| highlight.is_some_and(|(h, _)| h.contains(&i));
    // Most severe mark covering byte i (Some(true) = error, Some(false) = warning).
    let mark_at = |i: usize| {
        let mut found = None;
        for m in marks.iter().filter(|m| m.range.contains(&i)) {
            if m.severity == Severity::Error {
                return Some(true);
            }
            found = Some(false);
        }
        found
    };
    let hl_bg = highlight.map(|(_, sev)| match sev {
        Severity::Error => c.error_bg,
        Severity::Warning => c.warning_bg,
    });

    let r = row.range.clone();
    let mut start = r.start;
    while start < r.end {
        let key = (doc.classes[start], in_hl(start), mark_at(start));
        let mut end = start + 1;
        while end < r.end && (doc.classes[end], in_hl(end), mark_at(end)) == key {
            end += 1;
        }
        // Run boundaries fall on ASCII bytes or row ends, but guard char boundaries anyway.
        while end < r.end && !doc.text.is_char_boundary(end) {
            end += 1;
        }
        let (class, hl, mark) = key;
        let mut fmt = TextFormat::simple(font.clone(), class_color(class, c));
        if class == Class::Comment {
            fmt.italics = true;
        }
        if hl {
            fmt.background = hl_bg.unwrap_or_default();
        } else if let Some(is_error) = mark {
            fmt.background = if is_error { c.error_faint } else { c.warning_faint };
        }
        if let Some(is_error) = mark {
            fmt.underline = egui::Stroke::new(1.5, if is_error { c.marker } else { c.warning_fg });
        }
        job.append(&doc.text[start..end].replace('\t', "    "), 0.0, fmt);
        start = end;
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(range: Range<usize>, key: (usize, usize)) -> Mark {
        Mark { range, severity: Severity::Error, key, label: String::new() }
    }

    #[test]
    fn row_marks_finds_overlaps() {
        let mut v = CodeView::default();
        v.set_marks(vec![mark(50..60, (1, 0)), mark(5..12, (0, 0)), mark(5..12, (0, 1)), mark(30..45, (2, 0))]);
        let row = |r: Range<usize>| Row { line: 0, first: true, range: r };
        let keys = |v: &CodeView, r| v.row_marks(&row(r)).iter().map(|m| m.key).collect::<Vec<_>>();
        assert_eq!(keys(&v, 0..10), vec![(0, 0), (0, 1)]);
        assert_eq!(keys(&v, 10..35), vec![(0, 0), (0, 1), (2, 0)]);
        assert_eq!(keys(&v, 12..30), Vec::<(usize, usize)>::new());
        assert_eq!(keys(&v, 40..55), vec![(2, 0), (1, 0)]);
        v.set_marks_visible(false);
        assert!(keys(&v, 0..100).is_empty());
    }

    #[test]
    fn columns_map_to_bytes() {
        assert_eq!(byte_at_column("ab\tcd", 0), 0);
        assert_eq!(byte_at_column("ab\tcd", 3), 2, "inside the tab");
        assert_eq!(byte_at_column("ab\tcd", 6), 3);
        assert_eq!(byte_at_column("éa", 1), 2, "multi-byte char");
        assert_eq!(byte_at_column("ab", 99), 1);
    }
}
