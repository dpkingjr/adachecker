//! "Sortsite Eval": open a SortSite report exported to Excel and browse its worksheets.
//! Everything stays in the main window, on two tabs: "Open file" and "Worksheets"
//! (worksheet list on the left, data grid on the right).

use crate::spreadsheet::{self, Sheet, Workbook};
use crate::links::{self, Kind, LinkResult};
use crate::xlsx_edit::{self, Status, Statuses};
use std::collections::HashMap;
use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

/// Where the row-details pane is docked, relative to the grid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DetailsSide {
    #[default]
    Bottom,
    Right,
    Left,
}

impl DetailsSide {
    pub const ALL: [DetailsSide; 3] = [DetailsSide::Bottom, DetailsSide::Right, DetailsSide::Left];

    /// Value stored in the settings file.
    pub fn as_str(self) -> &'static str {
        match self {
            DetailsSide::Bottom => "bottom",
            DetailsSide::Right => "right",
            DetailsSide::Left => "left",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        DetailsSide::ALL.into_iter().find(|d| d.as_str() == s.trim())
    }

    fn label(self) -> &'static str {
        match self {
            DetailsSide::Bottom => "Below",
            DetailsSide::Right => "Right",
            DetailsSide::Left => "Left",
        }
    }

    fn hover(self) -> &'static str {
        match self {
            DetailsSide::Bottom => "Show row details below the grid",
            DetailsSide::Right => "Show row details to the right of the grid",
            DetailsSide::Left => "Show row details to the left of the grid",
        }
    }
}

/// The tabs of the Sortsite Eval pane.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Tab {
    #[default]
    File,
    Sheets,
}

#[derive(Default)]
pub struct SortsiteEval {
    tab: Tab,
    workbook: Option<Arc<Workbook>>,
    loading: Option<(PathBuf, Receiver<Result<Workbook, String>>)>,
    /// A file to open on the next frame (needs the egui context for repaints).
    pending_open: Option<PathBuf>,
    error: Option<String>,
    last_dir: Option<PathBuf>,
    /// Index of the worksheet shown in the grid.
    selected: usize,
    /// Treat the first row as column names (on by default; SortSite exports have headers).
    no_header_row: bool,
    filter: String,
    /// Selected row (index into the sheet's rows) shown in the details pane.
    selected_row: Option<usize>,
    /// Debug builds only: worksheet/row to select once the pending file has loaded.
    preselect: Option<(usize, Option<usize>)>,
    /// Where the row-details pane is docked (saved between runs by the app).
    details_side: DetailsSide,
    /// Set when the user picks a new side, so the app can save it right away.
    details_side_changed: bool,
    /// Row statuses (Fixed / On-Hold / N/A) for the open workbook.
    statuses: Statuses,
    /// Whether the "save as -fixed?" question has been asked for this workbook.
    asked_to_save: bool,
    /// Save to the -fixed file after every status change (the user said yes).
    autosave: bool,
    show_save_prompt: bool,
    saving: Option<Receiver<Result<PathBuf, String>>>,
    /// A status changed while a save was running; save again when it finishes.
    save_again: bool,
    last_save: Option<Result<PathBuf, String>>,
    /// Status changes not yet written to disk.
    unsaved: bool,
    /// Debug builds only: a status to set on the preselected row, and whether to accept
    /// the save prompt (for testing).
    debug_status: Option<Status>,
    debug_accept_save: bool,
    debug_check_link: bool,
    /// "Check Link" results per (worksheet, Excel row).
    link_checks: HashMap<(String, u32), LinkCheck>,
}

/// Finds the link to re-check for a SortSite "Broken Link" row: `None` if the row's
/// Guidelines value isn't "Broken Link", `Some(None)` if it is but no link was found.
/// The link comes from the "Target URL" column, or failing that the first URL in "Detail".
fn broken_link_target(header: &[String], row: &[String]) -> Option<Option<String>> {
    let col = |name: &str| header.iter().position(|h| h.trim().eq_ignore_ascii_case(name));
    let cell = |c: usize| row.get(c).map(|v| v.trim()).unwrap_or("");
    let guideline = col("Guidelines").map(cell)?;
    if !guideline.eq_ignore_ascii_case("broken link") {
        return None;
    }
    let detail = col("Detail").map(cell).unwrap_or("");
    let from_target = col("Target URL").map(cell).filter(|v| is_url(v)).map(str::to_string);
    let from_detail = || {
        detail
            .split_whitespace()
            .find(|t| t.starts_with("http://") || t.starts_with("https://"))
            .map(|t| t.trim_end_matches([',', ';', ')', '.', '"', '\'']).to_string())
    };
    let url = from_target.or_else(from_detail);
    // For "The target anchor does not exist" SortSite leaves the #fragment off the Target
    // URL and reports the missing anchor in Detail as "id=…". Put it back so the check
    // verifies the anchor, not just the page.
    let about_anchor = col("Description").map(cell).is_some_and(|d| d.to_ascii_lowercase().contains("anchor"));
    let url = match url {
        Some(u) if about_anchor && !u.contains('#') => {
            let id = detail
                .split(|c: char| c.is_whitespace() || c == ';' || c == ',')
                .find_map(|t| t.strip_prefix("id="))
                .filter(|id| !id.is_empty());
            Some(match id {
                Some(id) => format!("{u}#{id}"),
                None => u,
            })
        }
        other => other,
    };
    Some(url)
}

/// The column a broken link was taken from: "Target URL" when it holds a URL, otherwise
/// "Detail" when it contains one (matching `broken_link_target`).
fn link_column(header: &[String], row: &[String]) -> Option<usize> {
    let col = |name: &str| header.iter().position(|h| h.trim().eq_ignore_ascii_case(name));
    let cell = |c: usize| row.get(c).map(|v| v.trim()).unwrap_or("");
    col("Target URL")
        .filter(|&c| is_url(cell(c)))
        .or_else(|| col("Detail").filter(|&c| cell(c).contains("http://") || cell(c).contains("https://")))
}

/// Result of re-checking a broken link.
#[derive(Clone)]
struct LinkCheckOutcome {
    url: String,
    result: LinkResult,
    /// For links with a `#fragment`: whether that anchor exists on the page.
    anchor: Option<Result<bool, String>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tone {
    Good,
    Warn,
    Bad,
}

impl LinkCheckOutcome {
    /// The link works: 2xx (possibly after redirects), and its anchor exists if it has one.
    fn works(&self) -> bool {
        matches!(self.result.kind, Kind::Ok | Kind::Redirected) && matches!(self.anchor, None | Some(Ok(true)))
    }

    fn fragment(&self) -> String {
        self.url.split_once('#').map(|(_, f)| f.to_string()).unwrap_or_default()
    }

    fn message(&self) -> (String, Tone) {
        let r = &self.result;
        let status = r.status_text();
        if self.works() {
            let mut msg = if r.hops.is_empty() {
                format!("✔ Link works ({status})")
            } else {
                format!("✔ Link works ({status} after redirecting to {})", r.final_url)
            };
            if self.anchor.is_some() {
                msg.push_str(&format!(" and #{} exists", self.fragment()));
            }
            msg.push_str(" — marked Fixed.");
            return (msg, Tone::Good);
        }
        match (&r.kind, &self.anchor) {
            (Kind::Ok | Kind::Redirected, Some(Ok(false))) => {
                (format!("✖ The page loads ({status}) but the #{} anchor is still missing.", self.fragment()), Tone::Bad)
            }
            (Kind::Ok | Kind::Redirected, Some(Err(e))) => (format!("⚠ The page loads ({status}) but its anchor couldn't be checked: {e}"), Tone::Warn),
            (Kind::Restricted, _) => {
                (format!("⚠ The server refused the check ({status}). The link may work in a browser — not marked."), Tone::Warn)
            }
            (Kind::Error, _) => (format!("✖ Still broken: {}", r.reason), Tone::Bad),
            _ => (format!("✖ Still broken: {status}"), Tone::Bad),
        }
    }
}

enum LinkCheck {
    Running(Receiver<LinkCheckOutcome>),
    Done(LinkCheckOutcome),
}

/// Row fill shown in the app: dark shades in dark mode so text stays readable.
fn status_fill(status: Status, dark: bool) -> Color32 {
    match (status, dark) {
        (Status::Fixed, true) => Color32::from_rgb(34, 85, 45),
        (Status::OnHold, true) => Color32::from_rgb(95, 80, 15),
        (Status::NotApplicable, true) => Color32::from_rgb(72, 72, 72),
        (Status::Fixed, false) => Color32::from_rgb(198, 239, 206),
        (Status::OnHold, false) => Color32::from_rgb(255, 235, 156),
        (Status::NotApplicable, false) => Color32::from_rgb(217, 217, 217),
    }
}

/// The colour saved in the spreadsheet, for swatches in the menu.
fn excel_color(status: Status) -> Color32 {
    let hex = &status.argb()[2..];
    let v = u32::from_str_radix(hex, 16).unwrap_or(0);
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// Paints a table cell's background with the row's status, and marks the selected row
/// with lines above and below (so selection stays visible on coloured rows).
fn paint_cell(ui: &egui::Ui, status: Option<Status>, selected: bool) {
    let rect = ui.max_rect();
    if let Some(s) = status {
        ui.painter().rect_filled(rect, 0.0, status_fill(s, ui.visuals().dark_mode));
        if selected {
            let stroke = egui::Stroke::new(2.0, ui.visuals().selection.stroke.color);
            ui.painter().hline(rect.x_range(), rect.top() + 1.0, stroke);
            ui.painter().hline(rect.x_range(), rect.bottom() - 1.0, stroke);
        }
    }
}

fn muted(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode { Color32::from_rgb(170, 170, 170) } else { Color32::from_rgb(85, 85, 85) }
}

fn is_url(s: &str) -> bool {
    let l = s.trim_start().to_ascii_lowercase();
    (l.starts_with("http://") || l.starts_with("https://")) && !s.trim().contains(char::is_whitespace)
}

/// "1 row · 9 columns".
fn size_text(rows: usize, cols: usize) -> String {
    let plural = |n: usize, word: &str| if n == 1 { format!("1 {word}") } else { format!("{n} {word}s") };
    format!("{} · {}", plural(rows, "row"), plural(cols, "column"))
}

/// Number of data rows (excluding the header row when there is one).
fn data_rows(sheet: &Sheet, header: bool) -> usize {
    sheet.rows.len().saturating_sub(usize::from(header && !sheet.rows.is_empty()))
}

impl SortsiteEval {
    /// Opens `path` on the next frame (used by debug builds to preload a file).
    pub fn request_open(&mut self, path: PathBuf) {
        self.pending_open = Some(path);
    }

    pub fn details_side(&self) -> DetailsSide {
        self.details_side
    }

    /// Restores a saved choice (does not count as a change to save).
    pub fn set_details_side(&mut self, side: DetailsSide) {
        self.details_side = side;
    }

    /// A choice made by the user; flagged so the app saves it.
    pub fn choose_details_side(&mut self, side: DetailsSide) {
        if self.details_side != side {
            self.details_side = side;
            self.details_side_changed = true;
        }
    }

    /// True once after the user changed the dock side.
    pub fn take_details_side_changed(&mut self) -> bool {
        std::mem::take(&mut self.details_side_changed)
    }

    /// Debug builds only: after loading, set `status` on the preselected row and optionally
    /// answer "Save" to the prompt.
    pub fn debug_actions(&mut self, status: Option<Status>, accept_save: bool, check_link: bool) {
        self.debug_status = status;
        self.debug_accept_save = accept_save;
        self.debug_check_link = check_link;
    }

    /// Re-checks a broken link in the background; `frame` marks the row Fixed if it works.
    fn start_link_check(&mut self, sheet: &str, excel_row: u32, url: String, ctx: &egui::Context) {
        let key = (sheet.to_string(), excel_row);
        if matches!(self.link_checks.get(&key), Some(LinkCheck::Running(_))) {
            return;
        }
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = match links::build_client() {
                Ok(client) => links::check(&client, &url),
                Err(e) => LinkResult {
                    kind: Kind::Error,
                    status: None,
                    reason: format!("Could not create HTTP client: {e}"),
                    hops: Vec::new(),
                    final_url: url.clone(),
                    method: "-",
                    elapsed: std::time::Duration::ZERO,
                },
            };
            // Only look for the anchor once the page itself loads.
            let anchor = if matches!(result.kind, Kind::Ok | Kind::Redirected) {
                match links::check_anchor(&url) {
                    Ok(None) => None,
                    Ok(Some(found)) => Some(Ok(found)),
                    Err(e) => Some(Err(e)),
                }
            } else {
                None
            };
            let _ = tx.send(LinkCheckOutcome { url, result, anchor });
            ctx.request_repaint();
        });
        self.link_checks.insert(key, LinkCheck::Running(rx));
    }

    fn status_of(&self, sheet: &str, excel_row: u32) -> Option<Status> {
        self.statuses.get(sheet).and_then(|rows| rows.get(&excel_row)).copied()
    }

    fn set_status(&mut self, sheet: &str, excel_row: u32, status: Option<Status>, ctx: &egui::Context) {
        if self.status_of(sheet, excel_row) == status {
            return;
        }
        let rows = self.statuses.entry(sheet.to_string()).or_default();
        match status {
            Some(s) => {
                rows.insert(excel_row, s);
            }
            None => {
                rows.remove(&excel_row);
            }
        }
        if rows.is_empty() {
            self.statuses.remove(sheet);
        }
        self.unsaved = true;
        if !self.asked_to_save {
            // First status change for this workbook: ask before writing any file.
            self.asked_to_save = true;
            self.show_save_prompt = true;
        } else if self.autosave {
            self.start_save(ctx);
        }
    }

    fn save_target(&self) -> Option<PathBuf> {
        self.workbook.as_ref().map(|wb| xlsx_edit::fixed_path(&wb.path))
    }

    fn accept_save(&mut self, ctx: &egui::Context) {
        self.show_save_prompt = false;
        self.autosave = true;
        self.start_save(ctx);
    }

    /// Saves the workbook with the current statuses to the -fixed file, in the background.
    fn start_save(&mut self, ctx: &egui::Context) {
        let (Some(wb), Some(target)) = (&self.workbook, self.save_target()) else { return };
        let Some(original) = wb.original.clone() else { return };
        if self.saving.is_some() {
            self.save_again = true;
            return;
        }
        let statuses = self.statuses.clone();
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = xlsx_edit::save_with_statuses(&original, &statuses, &target).map(|()| target);
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.saving = Some(rx);
        self.unsaved = false;
    }

    /// Debug builds only: selects worksheet `sheet` (and optionally a row) after loading.
    pub fn preselect(&mut self, sheet: usize, row: Option<usize>) {
        self.preselect = Some((sheet, row));
    }

    fn open_path(&mut self, path: PathBuf, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        let p = path.clone();
        std::thread::spawn(move || {
            let _ = tx.send(spreadsheet::load(&p));
            ctx.request_repaint();
        });
        self.last_dir = path.parent().map(PathBuf::from);
        self.loading = Some((path, rx));
        self.error = None;
    }

    fn browse(&mut self, ctx: &egui::Context) {
        let mut dialog = rfd::FileDialog::new()
            .set_title("Open a SortSite Excel report")
            .add_filter("Spreadsheets", spreadsheet::EXTENSIONS)
            .add_filter("All files", &["*"]);
        if let Some(dir) = &self.last_dir {
            dialog = dialog.set_directory(dir);
        }
        if let Some(path) = dialog.pick_file() {
            self.open_path(path, ctx);
        }
    }

    fn select_sheet(&mut self, i: usize) {
        if self.selected != i {
            self.selected = i;
            self.filter.clear();
            self.selected_row = None;
        }
    }

    /// Per-frame work independent of which tool is shown: finishes loading a workbook.
    pub fn frame(&mut self, ctx: &egui::Context) {
        if let Some(path) = self.pending_open.take() {
            self.open_path(path, ctx);
        }
        if let Some((_, rx)) = &self.loading {
            if let Ok(result) = rx.try_recv() {
                self.loading = None;
                match result {
                    Ok(wb) => {
                        // A new workbook starts with the statuses saved in it, and asks again
                        // before saving.
                        self.statuses = wb.saved_statuses.clone();
                        self.link_checks.clear();
                        self.asked_to_save = false;
                        self.autosave = false;
                        self.show_save_prompt = false;
                        self.unsaved = false;
                        self.last_save = None;
                        self.workbook = Some(Arc::new(wb));
                        self.selected = usize::MAX; // force reset
                        self.select_sheet(0);
                        if let Some((sheet, row)) = self.preselect.take() {
                            self.select_sheet(sheet);
                            self.selected_row = row;
                            if let (Some(status), Some(r), Some(wb)) = (self.debug_status.take(), row, self.workbook.clone()) {
                                if let Some(sh) = wb.sheets.get(sheet) {
                                    self.set_status(&sh.name, (sh.first_row + r + 1) as u32, Some(status), ctx);
                                    if std::mem::take(&mut self.debug_accept_save) {
                                        self.accept_save(ctx);
                                    }
                                }
                            }
                            if std::mem::take(&mut self.debug_check_link) {
                                if let (Some(r), Some(wb)) = (row, self.workbook.clone()) {
                                    if let Some(sh) = wb.sheets.get(sheet) {
                                        if let (Some(header), Some(cells)) = (sh.rows.first(), sh.rows.get(r)) {
                                            if let Some(Some(url)) = broken_link_target(header, cells) {
                                                self.start_link_check(&sh.name, (sh.first_row + r + 1) as u32, url, ctx);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        self.tab = Tab::Sheets;
                    }
                    Err(e) => self.error = Some(e),
                }
            }
        }
        // Finished link checks: a link that works now marks its row Fixed.
        let mut now_working = Vec::new();
        for (key, check) in self.link_checks.iter_mut() {
            if let LinkCheck::Running(rx) = check {
                if let Ok(outcome) = rx.try_recv() {
                    if outcome.works() {
                        now_working.push(key.clone());
                    }
                    *check = LinkCheck::Done(outcome);
                }
            }
        }
        for (sheet, row) in now_working {
            self.set_status(&sheet, row, Some(Status::Fixed), ctx);
        }
        if let Some(rx) = &self.saving {
            if let Ok(result) = rx.try_recv() {
                self.saving = None;
                if result.is_err() {
                    self.unsaved = true;
                }
                self.last_save = Some(result);
                if std::mem::take(&mut self.save_again) {
                    self.start_save(ctx);
                }
            }
        }
    }

    /// "Save your changes?" — asked the first time a status is set on a workbook.
    fn save_prompt(&mut self, ctx: &egui::Context) {
        if !self.show_save_prompt {
            return;
        }
        let (Some(wb), Some(target)) = (self.workbook.clone(), self.save_target()) else { return };
        let target_name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let in_place = target == wb.path;
        let replaces = !in_place && target.exists();
        let mut accept = false;
        let mut decline = false;
        let modal = egui::Modal::new(egui::Id::unique("sortsite-save-prompt")).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.heading("Save your changes?");
            ui.add_space(6.0);
            if in_place {
                ui.label(format!("Save your row statuses to “{target_name}”?"));
            } else {
                ui.label(format!(
                    "Save a copy of the spreadsheet with your row statuses as “{target_name}”, in the same folder as “{}”?",
                    wb.file_name()
                ));
                ui.label(RichText::new("The original file is not changed.").small());
            }
            ui.add_space(4.0);
            ui.label("After this, every status change is saved to that file automatically.");
            if replaces {
                ui.add_space(4.0);
                let amber = if ui.visuals().dark_mode { Color32::from_rgb(255, 204, 102) } else { Color32::from_rgb(138, 76, 0) };
                ui.label(RichText::new(format!("⚠ “{target_name}” already exists and will be replaced.")).color(amber));
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new(format!("Save as {target_name}")).strong()).clicked() {
                    accept = true;
                }
                if ui.button("Not now").on_hover_text("Keep the changes only while the file is open. You can save later.").clicked() {
                    decline = true;
                }
            });
        });
        if accept {
            self.accept_save(ctx);
        } else if decline || modal.should_close() {
            self.show_save_prompt = false;
        }
    }

    /// The pane shown in the main window when "Sortsite Eval" is selected: a tab bar with
    /// "Open file" and "Worksheets", both inside the main window.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.save_prompt(&ui.ctx().clone());
        let wb = self.workbook.clone();
        let tab = if wb.is_some() { self.tab } else { Tab::File };
        egui::Panel::top("sortsite-tabs").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.selectable_label(tab == Tab::File, RichText::new("Open file").strong()).clicked() {
                    self.tab = Tab::File;
                }
                let sheets_label = match &wb {
                    Some(wb) => format!("Worksheets — {}", wb.file_name()),
                    None => "Worksheets".to_string(),
                };
                ui.add_enabled_ui(wb.is_some(), |ui| {
                    if ui
                        .selectable_label(tab == Tab::Sheets, RichText::new(sheets_label).strong())
                        .on_disabled_hover_text("Open a spreadsheet first")
                        .clicked()
                    {
                        self.tab = Tab::Sheets;
                    }
                });
            });
            ui.add_space(4.0);
        });
        match (tab, wb) {
            (Tab::Sheets, Some(wb)) => {
                egui::Panel::left("sheet-list")
                    .resizable(true)
                    .default_size(240.0)
                    .min_size(170.0)
                    .show(ui, |ui| self.sheet_list(ui, &wb));
                egui::CentralPanel::default().show(ui, |ui| self.sheet_grid(ui, &wb));
            }
            _ => {
                egui::CentralPanel::default().show(ui, |ui| self.file_pane(ui));
            }
        }
    }

    /// "Open file" tab: the Browse button and a summary of the open workbook.
    fn file_pane(&mut self, ui: &mut egui::Ui) {
        let muted = muted(ui);
        ui.add_space(6.0);
        ui.heading("Sortsite Eval");
        ui.label("Review a SortSite report that has been exported to Excel. Open the spreadsheet to browse its worksheets.");
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            let busy = self.loading.is_some();
            if ui
                .add_enabled(!busy, egui::Button::new(RichText::new("Browse for Excel spreadsheet…").strong()).min_size(egui::vec2(0.0, 30.0)))
                .on_hover_text(format!("Supported: {}", spreadsheet::EXTENSIONS.join(", ")))
                .clicked()
            {
                self.browse(ui.ctx());
            }
            if let Some((path, _)) = &self.loading {
                ui.spinner();
                ui.label(format!("Opening {}…", path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()));
            }
        });
        if let Some(e) = &self.error {
            ui.add_space(6.0);
            let red = if ui.visuals().dark_mode { Color32::from_rgb(255, 138, 128) } else { Color32::from_rgb(176, 0, 32) };
            ui.label(RichText::new(e).color(red));
        }

        let Some(wb) = self.workbook.clone() else {
            ui.add_space(24.0);
            ui.label(RichText::new("No spreadsheet open.").color(muted));
            return;
        };
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(6.0);
        egui::Grid::new("sortsite-file").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
            ui.strong("File");
            ui.label(wb.file_name());
            ui.end_row();
            ui.strong("Location");
            ui.label(RichText::new(wb.path.parent().map(|p| p.display().to_string()).unwrap_or_default()).color(muted));
            ui.end_row();
            ui.strong("Worksheets");
            ui.label(wb.sheets.len().to_string());
            ui.end_row();
        });
        ui.add_space(8.0);
        if ui.button("View worksheets").clicked() {
            self.tab = Tab::Sheets;
        }
        ui.add_space(10.0);
        ui.label(RichText::new("Worksheets (click one to view it)").strong());
        ui.add_space(4.0);
        let header = !self.no_header_row;
        egui::ScrollArea::vertical().id_salt("sortsite-sheet-summary").show(ui, |ui| {
            for (i, sheet) in wb.sheets.iter().enumerate() {
                let detail = match &sheet.error {
                    Some(e) => format!("Could not read: {e}"),
                    None => size_text(data_rows(sheet, header), sheet.width),
                };
                if ui.selectable_label(self.selected == i, format!("{}   —   {detail}", sheet.name)).clicked() {
                    self.select_sheet(i);
                    self.tab = Tab::Sheets;
                }
            }
        });
    }

    fn sheet_list(&mut self, ui: &mut egui::Ui, wb: &Workbook) {
        let muted = muted(ui);
        ui.add_space(10.0);
        ui.heading("Worksheets");
        ui.label(RichText::new(format!("{} in {}", wb.sheets.len(), wb.file_name())).small().color(muted));
        ui.add_space(6.0);
        ui.separator();
        let header = !self.no_header_row;
        let mut clicked = None;
        egui::ScrollArea::vertical().id_salt("sheet-list-scroll").auto_shrink([false, false]).show(ui, |ui| {
            for (i, sheet) in wb.sheets.iter().enumerate() {
                let marked = self.statuses.get(&sheet.name).map_or(0, |m| m.len());
                let subtitle = match &sheet.error {
                    Some(_) => "Could not be read".to_string(),
                    None if sheet.rows.is_empty() => "Empty".to_string(),
                    None if marked > 0 => format!("{} · {marked} marked", size_text(data_rows(sheet, header), sheet.width)),
                    None => size_text(data_rows(sheet, header), sheet.width),
                };
                let resp = crate::nav_tab(ui, self.selected == i, &sheet.name, &subtitle);
                if resp.clicked() {
                    clicked = Some(i);
                }
                // Up/Down arrows move between worksheets while one has keyboard focus.
                if resp.has_focus() {
                    let (up, down) = ui.input(|inp| (inp.key_pressed(egui::Key::ArrowUp), inp.key_pressed(egui::Key::ArrowDown)));
                    if down && i + 1 < wb.sheets.len() {
                        clicked = Some(i + 1);
                    } else if up && i > 0 {
                        clicked = Some(i - 1);
                    }
                }
                ui.add_space(4.0);
            }
        });
        if let Some(i) = clicked {
            self.select_sheet(i);
        }
    }

    /// Status counts for this worksheet and the save state of the -fixed file.
    fn status_bar(&mut self, ui: &mut egui::Ui, wb: &Workbook, sheet: &Sheet) {
        if wb.original.is_none() {
            return;
        }
        let muted = muted(ui);
        let counts: Vec<String> = Status::ALL
            .iter()
            .filter_map(|s| {
                let n = self.statuses.get(&sheet.name).map_or(0, |m| m.values().filter(|v| *v == s).count());
                (n > 0).then(|| format!("{} {n}", s.label()))
            })
            .collect();
        let mut save_now = false;
        let target_name = self.save_target().and_then(|t| t.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default();
        ui.horizontal(|ui| {
            if counts.is_empty() {
                ui.label(RichText::new("Click a row, then set its status in the details pane.").color(muted));
            } else {
                ui.label(RichText::new(counts.join(" · ")).color(muted));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let red = if ui.visuals().dark_mode { Color32::from_rgb(255, 138, 128) } else { Color32::from_rgb(176, 0, 32) };
                let green = if ui.visuals().dark_mode { Color32::from_rgb(120, 220, 140) } else { Color32::from_rgb(0, 110, 40) };
                if self.saving.is_some() {
                    ui.label("Saving…");
                    ui.spinner();
                } else if let Some(Err(e)) = &self.last_save {
                    if ui.button("Retry").clicked() {
                        save_now = true;
                    }
                    ui.label(RichText::new(format!("Save failed: {e}")).color(red));
                } else if self.unsaved && !self.autosave {
                    if ui
                        .button(format!("Save as {target_name}"))
                        .on_hover_text("Save now, and after every later status change")
                        .clicked()
                    {
                        save_now = true;
                    }
                    ui.label(RichText::new("Unsaved changes").color(muted));
                } else if let Some(Ok(path)) = &self.last_save {
                    if ui.small_button("Show in Finder").clicked() {
                        let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
                    }
                    ui.label(RichText::new(format!("✔ Saved to {target_name}")).color(green))
                        .on_hover_text(path.display().to_string());
                }
            });
        });
        if save_now {
            self.autosave = true;
            self.start_save(&ui.ctx().clone());
        }
    }

    fn sheet_grid(&mut self, ui: &mut egui::Ui, wb: &Workbook) {
        let muted = muted(ui);
        let Some(sheet) = wb.sheets.get(self.selected) else { return };
        ui.add_space(6.0);
        ui.heading(&sheet.name);
        if let Some(e) = &sheet.error {
            ui.label(format!("This worksheet could not be read: {e}"));
            return;
        }
        if sheet.rows.is_empty() || sheet.width == 0 {
            ui.label(RichText::new("This worksheet is empty.").color(muted));
            return;
        }

        let header = !self.no_header_row;
        let start = usize::from(header);
        let needle = self.filter.trim().to_lowercase();
        let visible: Vec<usize> = (start..sheet.rows.len())
            .filter(|&r| needle.is_empty() || sheet.rows[r].iter().any(|c| c.to_lowercase().contains(&needle)))
            .collect();

        ui.horizontal(|ui| {
            let mut header_on = header;
            if ui.checkbox(&mut header_on, "First row contains column names").changed() {
                self.no_header_row = !header_on;
                self.selected_row = None;
            }
            ui.separator();
            let lbl = ui.label("Filter rows:");
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("text to find in any column").desired_width(240.0))
                .labelled_by(lbl.id);
            if !self.filter.is_empty() && ui.small_button("✖").on_hover_text("Clear filter").clicked() {
                self.filter.clear();
            }
            ui.label(RichText::new(format!("Showing {} of {} rows", visible.len(), data_rows(sheet, header))).color(muted));
        });
        self.status_bar(ui, wb, sheet);
        ui.add_space(4.0);

        let column_title = |c: usize| -> String {
            let name = if header { sheet.rows[0].get(c).map(|s| s.trim()).unwrap_or("") } else { "" };
            if name.is_empty() { sheet.column_letter(c) } else { name.to_string() }
        };

        // Details of the selected row, every column in full (selectable for copying), docked
        // below, right or left of the grid like a browser's developer tools.
        if let Some(r) = self.selected_row.filter(|r| *r < sheet.rows.len()) {
            let side = self.details_side;
            let mut close = false;
            let mut chosen = None;
            let excel_row = (sheet.first_row + r + 1) as u32;
            let current = self.status_of(&sheet.name, excel_row);
            let editable = wb.original.is_some();
            let mut status_choice: Option<Option<Status>> = None;
            // "Check Link" for SortSite "Broken Link" rows (needs the header row for column names).
            let broken_link = if header { sheet.rows.first().and_then(|h| broken_link_target(h, &sheet.rows[r])) } else { None };
            // The field the button sits beside: the column the link was taken from.
            let link_col = if matches!(broken_link, Some(Some(_))) { sheet.rows.first().and_then(|h| link_column(h, &sheet.rows[r])) } else { None };
            let check_view: Option<Option<(String, Tone)>> = self.link_checks.get(&(sheet.name.clone(), excel_row)).map(|c| match c {
                LinkCheck::Running(_) => None,
                LinkCheck::Done(o) => Some(o.message()),
            });
            let mut start_check = false;
            let content = |ui: &mut egui::Ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.strong(format!("Row {}", sheet.first_row + r + 1));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Close").on_hover_text("Hide the row details").clicked() {
                            close = true;
                        }
                        ui.separator();
                        // Laid out right-to-left, so add the choices in reverse.
                        for option in DetailsSide::ALL.iter().rev() {
                            if ui
                                .selectable_label(side == *option, option.label())
                                .on_hover_text(option.hover())
                                .clicked()
                            {
                                chosen = Some(*option);
                            }
                        }
                        ui.label(RichText::new("Dock:").color(muted));
                    });
                });
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    let label = ui.label("Status:");
                    ui.add_enabled_ui(editable, |ui| {
                        let text = current.map_or("Set status…", Status::label);
                        egui::ComboBox::from_id_salt("row-status")
                            .selected_text(text)
                            .show_ui(ui, |ui| {
                                for option in Status::ALL {
                                    let mut job = egui::text::LayoutJob::default();
                                    job.append("■ ", 0.0, egui::TextFormat::simple(egui::FontId::proportional(14.0), excel_color(option)));
                                    job.append(option.label(), 0.0, egui::TextFormat::simple(egui::FontId::proportional(14.0), ui.visuals().text_color()));
                                    if ui.selectable_label(current == Some(option), job).clicked() {
                                        status_choice = Some(Some(option));
                                    }
                                }
                                if current.is_some() {
                                    ui.separator();
                                    if ui.selectable_label(false, "Clear status").clicked() {
                                        status_choice = Some(None);
                                    }
                                }
                            })
                            .response
                            .labelled_by(label.id)
                            .on_hover_text("Mark this row Fixed, On-Hold or N/A")
                            .on_disabled_hover_text("Statuses can be saved for .xlsx and .xlsm files only");
                    });
                });
                if matches!(broken_link, Some(None)) {
                    ui.label(RichText::new("Broken link, but no URL was found in Target URL or Detail to check.").small().color(muted));
                }
                ui.separator();
                egui::ScrollArea::vertical().id_salt("row-details-scroll").auto_shrink([false, false]).show(ui, |ui| {
                    let value_widget = |ui: &mut egui::Ui, value: &str| {
                        if is_url(value) {
                            ui.add(egui::Hyperlink::from_label_and_url(value, value.trim()).open_in_new_tab(true));
                        } else if value.is_empty() {
                            ui.label(RichText::new("—").color(muted));
                        } else {
                            ui.add(egui::Label::new(value).wrap());
                        }
                    };
                    // "Check Link" sits beside the URL being checked: on the same line as the
                    // URL in the wide (Below) layout, and beside the field name in the narrow
                    // side layouts, where a long URL can't share its line.
                    let link_url = broken_link.clone().flatten();
                    let is_link_field = |c: usize| link_url.is_some() && link_col == Some(c);
                    let check_button = |ui: &mut egui::Ui| -> bool {
                        let running = matches!(check_view, Some(None));
                        ui.add_enabled(!running, egui::Button::new(RichText::new("Check Link").strong()))
                            .on_hover_text(format!(
                                "Check whether {} works now, in the background. If it does, this row is marked Fixed.",
                                link_url.as_deref().unwrap_or("")
                            ))
                            .on_disabled_hover_text("Checking…")
                            .clicked()
                    };
                    let check_result = |ui: &mut egui::Ui| match &check_view {
                        Some(None) => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Checking the link…");
                            });
                        }
                        Some(Some((msg, tone))) => {
                            let dark = ui.visuals().dark_mode;
                            let color = match (tone, dark) {
                                (Tone::Good, true) => Color32::from_rgb(120, 220, 140),
                                (Tone::Good, false) => Color32::from_rgb(0, 110, 40),
                                (Tone::Warn, true) => Color32::from_rgb(255, 204, 102),
                                (Tone::Warn, false) => Color32::from_rgb(138, 76, 0),
                                (Tone::Bad, true) => Color32::from_rgb(255, 138, 128),
                                (Tone::Bad, false) => Color32::from_rgb(176, 0, 32),
                            };
                            ui.add(egui::Label::new(RichText::new(msg).color(color)).wrap());
                        }
                        None => {}
                    };
                    if side == DetailsSide::Bottom {
                        // Wide pane: names and values side by side.
                        egui::Grid::new("row-details-grid").num_columns(2).spacing([16.0, 6.0]).striped(true).show(ui, |ui| {
                            for c in 0..sheet.width {
                                let value = sheet.rows[r].get(c).map(String::as_str).unwrap_or("");
                                ui.label(RichText::new(column_title(c)).strong());
                                if is_link_field(c) {
                                    ui.vertical(|ui| {
                                        ui.horizontal(|ui| {
                                            if check_button(ui) {
                                                start_check = true;
                                            }
                                            value_widget(ui, value);
                                        });
                                        check_result(ui);
                                    });
                                } else {
                                    value_widget(ui, value);
                                }
                                ui.end_row();
                            }
                        });
                    } else {
                        // Narrow side pane: each name above its value.
                        for c in 0..sheet.width {
                            let value = sheet.rows[r].get(c).map(String::as_str).unwrap_or("");
                            if is_link_field(c) {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(column_title(c)).strong());
                                    if check_button(ui) {
                                        start_check = true;
                                    }
                                });
                                value_widget(ui, value);
                                check_result(ui);
                            } else {
                                ui.label(RichText::new(column_title(c)).strong());
                                value_widget(ui, value);
                            }
                            ui.add_space(8.0);
                        }
                    }
                });
            };
            match side {
                DetailsSide::Bottom => {
                    egui::Panel::bottom("row-details-bottom").resizable(true).default_size(220.0).min_size(80.0).show(ui, content);
                }
                DetailsSide::Right => {
                    egui::Panel::right("row-details-right").resizable(true).default_size(380.0).min_size(220.0).show(ui, content);
                }
                DetailsSide::Left => {
                    egui::Panel::left("row-details-left").resizable(true).default_size(380.0).min_size(220.0).show(ui, content);
                }
            }
            if close {
                self.selected_row = None;
            }
            if let Some(option) = chosen {
                self.choose_details_side(option);
            }
            if let Some(status) = status_choice {
                self.set_status(&sheet.name, excel_row, status, &ui.ctx().clone());
            }
            if start_check {
                if let Some(Some(url)) = broken_link.clone() {
                    self.start_link_check(&sheet.name, excel_row, url, &ui.ctx().clone());
                }
            }
        }

        if visible.is_empty() {
            ui.label(RichText::new("No rows match the filter.").color(muted));
            return;
        }

        // Size columns to their content (header + a sample of rows), within limits.
        let char_w = 7.2;
        let widths: Vec<f32> = (0..sheet.width)
            .map(|c| {
                let longest = std::iter::once(column_title(c).chars().count())
                    .chain(visible.iter().take(200).map(|&r| sheet.rows[r].get(c).map(|s| s.chars().count()).unwrap_or(0)))
                    .max()
                    .unwrap_or(4);
                (longest as f32 * char_w + 18.0).clamp(64.0, 380.0)
            })
            .collect();

        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 8.0;
        let mut clicked_row = None;
        let sheet_statuses = self.statuses.get(&sheet.name).cloned().unwrap_or_default();
        egui::ScrollArea::horizontal().id_salt(("grid-hscroll", self.selected)).auto_shrink([false, false]).show(ui, |ui| {
            // No gaps between cells, so a row's status colour is continuous; cells pad themselves.
            ui.spacing_mut().item_spacing.x = 0.0;
            let mut table = TableBuilder::new(ui)
                .id_salt(("sheet-grid", self.selected, header))
                .striped(true)
                .resizable(true)
                .sense(egui::Sense::click())
                .auto_shrink([false, false])
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(104.0));
            for w in &widths {
                table = table.column(Column::initial(*w).at_least(40.0).clip(true));
            }
            table
                .header(row_h + 2.0, |mut h| {
                    h.col(|ui| {
                        ui.add_space(6.0);
                        ui.label(RichText::new("Row").strong().color(muted));
                    });
                    for c in 0..sheet.width {
                        h.col(|ui| {
                            ui.add_space(6.0);
                            ui.add(egui::Label::new(RichText::new(column_title(c)).strong()).truncate())
                                .on_hover_text(format!("Column {}", sheet.column_letter(c)));
                        });
                    }
                })
                .body(|body| {
                    body.rows(row_h, visible.len(), |mut row| {
                        let r = visible[row.index()];
                        let selected = self.selected_row == Some(r);
                        let status = sheet_statuses.get(&((sheet.first_row + r + 1) as u32)).copied();
                        row.set_selected(selected);
                        row.col(|ui| {
                            paint_cell(ui, status, selected);
                            ui.add_space(6.0);
                            ui.add(egui::Label::new(RichText::new((sheet.first_row + r + 1).to_string()).color(muted)).selectable(false));
                            // The status as text too, so colour is never the only cue.
                            if let Some(s) = status {
                                ui.add_space(6.0);
                                ui.add(egui::Label::new(RichText::new(s.label()).strong()).selectable(false));
                            }
                        });
                        for c in 0..sheet.width {
                            let text = sheet.rows[r].get(c).map(String::as_str).unwrap_or("");
                            row.col(|ui| {
                                paint_cell(ui, status, selected);
                                ui.add_space(6.0);
                                if is_url(text) {
                                    ui.add(egui::Hyperlink::from_label_and_url(text, text.trim()).open_in_new_tab(true))
                                        .on_hover_text(format!("{}\n\nClick to open in your browser", text.trim()));
                                } else if !text.is_empty() {
                                    let one_line = text.replace(['\n', '\r'], " ⏎ ");
                                    let resp = ui.add(egui::Label::new(one_line).truncate().selectable(false));
                                    if text.chars().count() > 40 || text.contains('\n') {
                                        resp.on_hover_ui(|ui| {
                                            ui.set_max_width(480.0);
                                            ui.label(text);
                                        });
                                    }
                                }
                            });
                        }
                        if row.response().clicked() {
                            clicked_row = Some(r);
                        }
                    });
                });
        });
        if let Some(r) = clicked_row {
            self.selected_row = if self.selected_row == Some(r) { None } else { Some(r) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_urls() {
        assert!(is_url("https://library.louisville.edu/archives/"));
        assert!(is_url("  http://x.test/a?b=1 "));
        assert!(!is_url("Link: https://example.com"));
        assert!(!is_url("https://a.test/ and more"));
        assert!(!is_url("ftp://x"));
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn finds_broken_link_targets() {
        let header = strings(&["Guidelines", "Priority", "Detail", "URL", "Target URL"]);
        let row = |g: &str, detail: &str, target: &str| strings(&[g, "1", detail, "https://page.example/", target]);
        // SortSite pads values with spaces; Target URL wins over Detail.
        assert_eq!(
            broken_link_target(&header, &row("Broken Link  ", "Broken link: https://d.example/x 404", " https://t.example/a#b ")),
            Some(Some("https://t.example/a#b".into()))
        );
        // Falls back to the first URL in Detail.
        assert_eq!(
            broken_link_target(&header, &row("Broken Link", "Link: https://d.example/x, 403", "")),
            Some(Some("https://d.example/x".into()))
        );
        assert_eq!(broken_link_target(&header, &row("Broken Link", "no link here", "")), Some(None));
        // SortSite's missing-anchor rows: the anchor id is in Detail, not the Target URL.
        let header2 = strings(&["Guidelines", "Description", "Detail", "Target URL"]);
        assert_eq!(
            broken_link_target(
                &header2,
                &strings(&[
                    "Broken Link  ",
                    "This link is broken. The target anchor does not exist or is commented out.",
                    " id=s-lg-box-wrapper-42400942",
                    "https://library.louisville.edu/c.php?g=158777&p=11471962",
                ])
            ),
            Some(Some("https://library.louisville.edu/c.php?g=158777&p=11471962#s-lg-box-wrapper-42400942".into()))
        );
        assert_eq!(broken_link_target(&header, &row("WCAG 2.1 A 4.1.1", "", "https://t.example/")), None);
        // The button sits beside the column the link came from.
        assert_eq!(link_column(&header, &row("Broken Link", "Broken link: https://d.example/x", "https://t.example/")), Some(4));
        assert_eq!(link_column(&header, &row("Broken Link", "Broken link: https://d.example/x", "")), Some(2));
        assert_eq!(link_column(&header, &row("Broken Link", "nothing", "")), None);
        // A sheet without a Guidelines column (e.g. "Known Issues") never offers the button.
        assert_eq!(broken_link_target(&strings(&["Priority", "Target URL"]), &strings(&["1", "https://t.example/"])), None);
    }

    fn outcome(kind: Kind, status: Option<u16>, url: &str, anchor: Option<Result<bool, String>>) -> LinkCheckOutcome {
        LinkCheckOutcome {
            url: url.into(),
            result: LinkResult {
                kind,
                status,
                reason: if status == Some(200) { "OK".into() } else { "Not Found".into() },
                hops: vec![],
                final_url: url.into(),
                method: "HEAD",
                elapsed: std::time::Duration::ZERO,
            },
            anchor,
        }
    }

    #[test]
    fn decides_when_a_link_counts_as_fixed() {
        assert!(outcome(Kind::Ok, Some(200), "https://x/", None).works());
        assert!(outcome(Kind::Redirected, Some(200), "https://x/", None).works());
        assert!(outcome(Kind::Ok, Some(200), "https://x/#a", Some(Ok(true))).works());
        let missing_anchor = outcome(Kind::Ok, Some(200), "https://x/#gone", Some(Ok(false)));
        assert!(!missing_anchor.works());
        assert!(missing_anchor.message().0.contains("#gone anchor is still missing"));
        assert!(!outcome(Kind::Ok, Some(200), "https://x/#a", Some(Err("timeout".into()))).works());
        assert!(!outcome(Kind::Broken, Some(404), "https://x/", None).works());
        let restricted = outcome(Kind::Restricted, Some(403), "https://x/", None);
        assert!(!restricted.works(), "a 403 is not proof the link is fixed");
        assert_eq!(restricted.message().1, Tone::Warn);
    }

    #[test]
    fn details_side_round_trips() {
        for side in DetailsSide::ALL {
            assert_eq!(DetailsSide::parse(side.as_str()), Some(side));
        }
        assert_eq!(DetailsSide::parse(" right\n"), Some(DetailsSide::Right));
        assert_eq!(DetailsSide::parse("top"), None);
        assert_eq!(DetailsSide::default(), DetailsSide::Bottom);
    }

    #[test]
    fn choosing_a_side_is_flagged_for_saving_once() {
        let mut s = SortsiteEval::default();
        s.set_details_side(DetailsSide::Right);
        assert!(!s.take_details_side_changed(), "restoring a saved value is not a change");
        s.choose_details_side(DetailsSide::Right);
        assert!(!s.take_details_side_changed(), "same side again is not a change");
        s.choose_details_side(DetailsSide::Left);
        assert!(s.take_details_side_changed());
        assert!(!s.take_details_side_changed(), "flag is consumed");
        assert_eq!(s.details_side(), DetailsSide::Left);
    }

    #[test]
    fn size_text_pluralizes() {
        assert_eq!(size_text(1, 9), "1 row · 9 columns");
        assert_eq!(size_text(28, 1), "28 rows · 1 column");
        assert_eq!(size_text(0, 0), "0 rows · 0 columns");
    }

    #[test]
    fn counts_data_rows() {
        let sheet = Sheet {
            name: "s".into(),
            rows: vec![vec!["h".into()], vec!["1".into()], vec!["2".into()]],
            width: 1,
            first_row: 0,
            first_col: 0,
            error: None,
        };
        assert_eq!(data_rows(&sheet, true), 2);
        assert_eq!(data_rows(&sheet, false), 3);
        let empty = Sheet { rows: vec![], ..sheet };
        assert_eq!(data_rows(&empty, true), 0);
    }
}
