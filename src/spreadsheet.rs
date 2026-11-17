//! Loading Excel/OpenDocument spreadsheets (via `calamine`) into plain text grids.

use crate::xlsx_edit::{self, Statuses};
use calamine::{open_workbook_auto, Data, Reader};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// File extensions offered in the Open dialog.
pub const EXTENSIONS: &[&str] = &["xlsx", "xlsm", "xlsb", "xls", "ods"];

pub struct Sheet {
    pub name: String,
    /// Cell text, row-major. Every row has `width` cells.
    pub rows: Vec<Vec<String>>,
    pub width: usize,
    /// 0-based row index of the first row in `rows` (for Excel row numbers).
    pub first_row: usize,
    /// 0-based column index of the first column in `rows` (for A/B/C column letters).
    pub first_col: usize,
    /// Set if this worksheet could not be read (others may still be fine).
    pub error: Option<String>,
}

impl Sheet {
    /// Excel-style letter of display column `i` ("A", …, "Z", "AA", …).
    pub fn column_letter(&self, i: usize) -> String {
        column_name(self.first_col + i)
    }
}

pub struct Workbook {
    pub path: PathBuf,
    pub sheets: Vec<Sheet>,
    /// The file's bytes as opened, for `.xlsx`/`.xlsm` (the base for saving statuses).
    pub original: Option<Arc<Vec<u8>>>,
    /// Row statuses already saved in the file (e.g. when reopening a `-fixed` copy).
    pub saved_statuses: Statuses,
}

impl Workbook {
    pub fn file_name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

/// Excel column name for a 0-based index.
pub fn column_name(mut i: usize) -> String {
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

/// Display text for a cell.
fn cell_text(d: &Data) -> String {
    match d {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        // Whole numbers without ".0"; others as Rust's shortest round-trip form.
        Data::Float(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", *f as i64),
        Data::DateTime(dt) => match dt.as_datetime() {
            // "2026-11-17 09:30:00"; dates without a time of day show as just the date.
            Some(t) => {
                let text = t.to_string();
                text.strip_suffix(" 00:00:00").map(str::to_string).unwrap_or(text)
            }
            None => dt.to_string(),
        },
        other => other.to_string(),
    }
}

/// Reads every worksheet of the workbook at `path`.
pub fn load(path: &Path) -> Result<Workbook, String> {
    let mut wb = open_workbook_auto(path).map_err(|e| format!("Could not open {}: {e}", path.display()))?;
    let names = wb.sheet_names();
    if names.is_empty() {
        return Err("The workbook has no worksheets.".into());
    }
    let sheets = names
        .into_iter()
        .map(|name| match wb.worksheet_range(&name) {
            Ok(range) => {
                let (first_row, first_col) = range.start().map(|(r, c)| (r as usize, c as usize)).unwrap_or((0, 0));
                let rows: Vec<Vec<String>> = range.rows().map(|r| r.iter().map(cell_text).collect()).collect();
                let width = rows.iter().map(Vec::len).max().unwrap_or(0);
                Sheet { name, rows, width, first_row, first_col, error: None }
            }
            Err(e) => Sheet { name, rows: Vec::new(), width: 0, first_row: 0, first_col: 0, error: Some(e.to_string()) },
        })
        .collect();
    let (original, saved_statuses) = if xlsx_edit::is_editable(path) {
        let bytes = std::fs::read(path).map_err(|e| format!("Could not read {}: {e}", path.display()))?;
        let statuses = xlsx_edit::read_statuses(&bytes).unwrap_or_default();
        (Some(Arc::new(bytes)), statuses)
    } else {
        (None, Statuses::new())
    };
    Ok(Workbook { path: path.to_path_buf(), sheets, original, saved_statuses })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_xlsxwriter::{ExcelDateTime, Format, Workbook as XlsxWriter};

    #[test]
    fn column_names() {
        let names: Vec<_> = [0, 1, 25, 26, 27, 51, 52, 701, 702].iter().map(|&i| column_name(i)).collect();
        assert_eq!(names, ["A", "B", "Z", "AA", "AB", "AZ", "BA", "ZZ", "AAA"]);
    }

    #[test]
    fn loads_all_worksheets_with_typed_cells() {
        let dir = std::env::temp_dir().join(format!("adachecker-xlsx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("report.xlsx");

        let mut book = XlsxWriter::new();
        let s1 = book.add_worksheet().set_name("Broken links").unwrap();
        for (c, h) in ["Guidelines", "Priority", "Description", "URL"].iter().enumerate() {
            s1.write_string(0, c as u16, *h).unwrap();
        }
        s1.write_string(1, 0, "Broken Link").unwrap();
        s1.write_number(1, 1, 1.0).unwrap();
        s1.write_string(1, 2, "This link returns a 404, \"Not Found\"").unwrap();
        s1.write_string(1, 3, "https://example.com/missing").unwrap();
        s1.write_number(2, 1, 2.5).unwrap();
        s1.write_boolean(2, 2, true).unwrap();
        let date = ExcelDateTime::from_ymd(2026, 11, 17).unwrap();
        s1.write_datetime_with_format(2, 3, &date, &Format::new().set_num_format("yyyy-mm-dd")).unwrap();
        // A sheet whose data starts at C3, and an empty sheet.
        let s2 = book.add_worksheet().set_name("Offset").unwrap();
        s2.write_string(2, 2, "starts at C3").unwrap();
        book.add_worksheet().set_name("Empty").unwrap();
        book.save(&path).unwrap();

        let wb = load(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(wb.file_name(), "report.xlsx");
        let names: Vec<_> = wb.sheets.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Broken links", "Offset", "Empty"]);

        let s = &wb.sheets[0];
        assert_eq!((s.rows.len(), s.width), (3, 4));
        assert_eq!(s.rows[0], ["Guidelines", "Priority", "Description", "URL"]);
        assert_eq!(s.rows[1], ["Broken Link", "1", "This link returns a 404, \"Not Found\"", "https://example.com/missing"]);
        assert_eq!(s.rows[2], ["", "2.5", "true", "2026-11-17"]);

        let off = &wb.sheets[1];
        assert_eq!(off.column_letter(0), "C", "column letters follow the sheet's real position");
        assert_eq!(off.first_row, 2, "row numbers too (Excel row 3)");
        assert_eq!(off.rows, [["starts at C3"]]);

        assert!(wb.sheets[2].rows.is_empty() && wb.sheets[2].error.is_none());
    }

    #[test]
    fn reports_unreadable_files() {
        let err = load(Path::new("/nonexistent/file.xlsx")).err().unwrap();
        assert!(err.starts_with("Could not open"), "{err}");
    }
}
