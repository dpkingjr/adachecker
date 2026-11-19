mod batch;
mod checks;
mod code_view;
mod link_checker_ui;
mod links;
mod preview;
mod scanner;
mod sortsite_ui;
mod source;
mod spreadsheet;
mod xlsx_edit;

use checks::{Level, Outcome, Severity, Version};
use eframe::egui::{self, Color32, RichText};
use batch::BatchScan;
use code_view::{CodeView, Mark};
use link_checker_ui::LinkChecker;
use sortsite_ui::{DetailsSide, SortsiteEval};

/// Settings-file key for where the Sortsite Eval row-details pane is docked.
const DETAILS_SIDE_KEY: &str = "sortsite.details_side";
use preview::Preview;
use scanner::{link_status, LinkBucket, Report};

/// Settings-file keys for Sortsite Eval's "reopen the last spreadsheet" option.
const SORTSITE_REOPEN_KEY: &str = "sortsite.reopen_last";
const SORTSITE_LAST_FILE_KEY: &str = "sortsite.last_file";

/// Settings-file key for the WCAG Scanner's "Check links" toggle.
const CHECK_LINKS_KEY: &str = "accessibility.check_links";
use std::sync::mpsc::{channel, Receiver};

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ADA Checker")
            .with_inner_size([1500.0, 900.0])
            .with_min_inner_size([1000.0, 520.0]),
        // Only explicitly saved settings persist (see `App::save`), not window geometry.
        persist_window: false,
        ..Default::default()
    };
    eframe::run_native(
        "ADA Checker",
        options,
        Box::new(|cc| {
            let mut app = App::default();
            // Restore saved settings.
            if let Some(side) = cc.storage.and_then(|s| s.get_string(DETAILS_SIDE_KEY)).and_then(|v| DetailsSide::parse(&v)) {
                app.sortsite.set_details_side(side);
            }
            if let Some(v) = cc.storage.and_then(|s| s.get_string(CHECK_LINKS_KEY)) {
                app.check_links = app.check_links || v == "true";
            }
            let reopen = cc.storage.and_then(|s| s.get_string(SORTSITE_REOPEN_KEY)).is_some_and(|v| v == "true");
            let last_file = cc.storage.and_then(|s| s.get_string(SORTSITE_LAST_FILE_KEY)).filter(|p| !p.is_empty()).map(std::path::PathBuf::from);
            app.sortsite.restore_settings(reopen, last_file);
            // Debug builds only: ADA_CHOOSE_SIDE=right acts as if the user then picked that
            // dock side (applied after restoring, like a real click would be).
            if cfg!(debug_assertions) {
                if let Some(side) = std::env::var("ADA_CHOOSE_SIDE").ok().and_then(|v| DetailsSide::parse(&v)) {
                    app.sortsite.choose_details_side(side);
                }
            }
            // Optional URL argument: `adachecker https://example.com` scans on launch.
            // (macOS may pass a `-psn_…` process-serial argument when launched from Finder.)
            if let Some(url) = std::env::args().skip(1).find(|a| !a.starts_with("-psn")) {
                app.url = url;
                app.scan_on_start = true;
                // A URL on the command line is scanned, so show the WCAG Scanner.
                if std::env::var_os("ADA_TOOL").is_none() {
                    app.tool = Tool::Accessibility;
                }
            }
            Ok(Box::new(app))
        }),
    )
}

/// The tools in the left-hand navigation.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Accessibility,
    Links,
    Sortsite,
}

/// Where a selection came from. Selecting in one view reveals the element in the other.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    List,
    Page,
    Source,
}

/// WCAG Scanner input: one address, or a list of pages.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ScanMode {
    Single,
    List,
}

/// The tabs of the results area.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ResultsTab {
    Wcag,
    Links,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SideTab {
    Page,
    Source,
}

enum ScanState {
    Idle,
    Scanning,
    Done(Report),
    Failed(String),
}

struct App {
    scan_mode: ScanMode,
    /// "List of pages" scanning.
    batch: BatchScan,
    /// The list page whose full results are shown in the one-page view, if any.
    viewing_batch: Option<usize>,
    /// Debug builds only: ADA_OPEN_BATCH=n opens list page n once it has been scanned.
    debug_open_batch: Option<usize>,
    tool: Tool,
    link_checker: LinkChecker,
    sortsite: SortsiteEval,
    url: String,
    version: Version,
    level: Level,
    state: ScanState,
    rx: Option<Receiver<Result<Report, String>>>,
    show_fail: bool,
    show_review: bool,
    show_pass: bool,
    copied: bool,
    preview: Preview,
    code_view: CodeView,
    side_tab: SideTab,
    show_preview: bool,
    /// Mark every finding in the page preview and HTML source.
    show_all_marks: bool,
    /// Frames left in which the results list keeps scrolling the selection into view.
    reveal_frames: u8,
    /// Debug builds only: ADA_CLICK_BADGE=1 clicks the first page badge after a scan.
    debug_click_badge: bool,
    /// "Check links" toggle: check every link on the scanned page (saved between runs).
    check_links: bool,
    check_links_changed: bool,
    /// Background check of the page's link addresses (each address once).
    link_run: Option<links::LinkRun<links::CheckedLink>>,
    link_urls: Vec<String>,
    link_results: Vec<Option<links::CheckedLink>>,
    /// For each `report.links` entry, its index in `link_urls` (None for in-page links).
    link_index: Vec<Option<usize>>,
    /// Results area: the WCAG findings or the list of the page's links.
    results_tab: ResultsTab,
    link_filter: String,
    links_only_problems: bool,
    /// Selected link (index into `report.links`).
    selected_link: Option<usize>,
    /// Debug builds only: ADA_SELECT_LINK=n selects page link n once the link check ends.
    debug_select_link: Option<usize>,
    /// Selected finding: (index into report.results, index into its findings).
    selected: Option<(usize, usize)>,
    scan_on_start: bool,
    /// Debug builds only: ADA_SELECT_FIRST=1 selects the first element finding after a scan.
    auto_select: bool,
}

impl Default for App {
    fn default() -> Self {
        Self {
            // Debug builds only: ADA_TOOL=links opens the Link Checker, and ADA_LINKS_FILE=path
            // pre-fills it from a file and starts checking (for testing).
            tool: match std::env::var("ADA_TOOL").as_deref() {
                Ok("links") if cfg!(debug_assertions) => Tool::Links,
                Ok("sortsite") if cfg!(debug_assertions) => Tool::Sortsite,
                Ok("scanner") if cfg!(debug_assertions) => Tool::Accessibility,
                // The app opens on Sortsite Eval.
                _ => Tool::Sortsite,
            },
            // Debug builds only: ADA_XLSX=path opens that spreadsheet on launch (for testing).
            sortsite: {
                let mut s = SortsiteEval::default();
                if cfg!(debug_assertions) {
                    if let Some(p) = std::env::var_os("ADA_XLSX") {
                        s.request_open(p.into());
                        // ADA_XLSX_SHEET=n / ADA_XLSX_ROW=n select a worksheet / row (0-based).
                        let num = |k: &str| std::env::var(k).ok().and_then(|v| v.parse().ok());
                        if let Some(sheet) = num("ADA_XLSX_SHEET") {
                            s.preselect(sheet, num("ADA_XLSX_ROW"));
                        }
                        // ADA_SET_STATUS=fixed|onhold|na|broken sets it on that row; ADA_ACCEPT_SAVE=1
                        // answers "Save" to the prompt.
                        let status = match std::env::var("ADA_SET_STATUS").as_deref() {
                            Ok("fixed") => Some(xlsx_edit::Status::Fixed),
                            Ok("onhold") => Some(xlsx_edit::Status::OnHold),
                            Ok("na") => Some(xlsx_edit::Status::NotApplicable),
                            Ok("broken") => Some(xlsx_edit::Status::Broken),
                            _ => None,
                        };
                        // ADA_SORT=3:desc sorts by data column 3 (0-based).
                        if let Some((col, dir)) = std::env::var("ADA_SORT").ok().and_then(|v| v.split_once(':').map(|(c, d)| (c.to_string(), d.to_string()))) {
                            if let Ok(col) = col.parse() {
                                s.debug_sort(col, dir != "desc");
                            }
                        }
                        // ADA_SET_NOTE=text sets that row's note (as if typed, then the box left).
                        if let Ok(text) = std::env::var("ADA_SET_NOTE") {
                            s.debug_note(text);
                        }
                        // ADA_ADA_CHECK=1 presses "ADA Check" on that row.
                        if std::env::var_os("ADA_ADA_CHECK").is_some() {
                            s.debug_ada_check();
                        }
                        // ADA_CHECK_LINK=1 presses "Check Link" on that row.
                        s.debug_actions(status, std::env::var_os("ADA_ACCEPT_SAVE").is_some(), std::env::var_os("ADA_CHECK_LINK").is_some());
                    }
                }
                s
            },
            link_checker: {
                let mut lc = LinkChecker::default();
                if cfg!(debug_assertions) {
                    if let Some(text) = std::env::var_os("ADA_LINKS_FILE").and_then(|p| std::fs::read_to_string(p).ok()) {
                        lc.load(text, true);
                    }
                }
                lc
            },
            // Debug builds only: ADA_SCAN_LIST=file pre-fills the list and scans it.
            scan_mode: if cfg!(debug_assertions) && std::env::var_os("ADA_SCAN_LIST").is_some() { ScanMode::List } else { ScanMode::Single },
            batch: {
                let mut b = BatchScan::default();
                if cfg!(debug_assertions) {
                    if let Some(text) = std::env::var_os("ADA_SCAN_LIST").and_then(|p| std::fs::read_to_string(p).ok()) {
                        b.load(text, true);
                    }
                }
                b
            },
            viewing_batch: None,
            debug_open_batch: if cfg!(debug_assertions) { std::env::var("ADA_OPEN_BATCH").ok().and_then(|v| v.parse().ok()) } else { None },
            url: String::new(),
            // WCAG 2.1 AA is the standard referenced by the ADA Title II rule.
            version: Version::V2_1,
            level: Level::AA,
            state: ScanState::Idle,
            rx: None,
            show_fail: true,
            show_review: true,
            show_pass: false,
            copied: false,
            preview: Preview::new(),
            code_view: CodeView::default(),
            // Debug builds only: ADA_SOURCE_TAB=1 opens the HTML source tab (for testing).
            side_tab: if cfg!(debug_assertions) && std::env::var_os("ADA_SOURCE_TAB").is_some() {
                SideTab::Source
            } else {
                SideTab::Page
            },
            show_preview: true,
            show_all_marks: true,
            reveal_frames: 0,
            debug_click_badge: cfg!(debug_assertions) && std::env::var_os("ADA_CLICK_BADGE").is_some(),
            // Debug builds only: ADA_CHECK_LINKS=1 turns the toggle on (without saving it).
            check_links: cfg!(debug_assertions) && std::env::var_os("ADA_CHECK_LINKS").is_some(),
            check_links_changed: false,
            link_run: None,
            link_urls: Vec::new(),
            link_results: Vec::new(),
            link_index: Vec::new(),
            // Debug builds only: ADA_RESULTS_TAB=links opens the Links tab.
            results_tab: if cfg!(debug_assertions) && std::env::var("ADA_RESULTS_TAB").as_deref() == Ok("links") {
                ResultsTab::Links
            } else {
                ResultsTab::Wcag
            },
            link_filter: String::new(),
            links_only_problems: false,
            selected_link: None,
            debug_select_link: if cfg!(debug_assertions) { std::env::var("ADA_SELECT_LINK").ok().and_then(|v| v.parse().ok()) } else { None },
            selected: None,
            scan_on_start: false,
            auto_select: cfg!(debug_assertions) && std::env::var_os("ADA_SELECT_FIRST").is_some(),
        }
    }
}

/// Status colours chosen to keep ≥ 4.5:1 contrast against egui's light and dark panels.
struct Palette {
    fail: Color32,
    review: Color32,
    pass: Color32,
    muted: Color32,
}

fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            fail: Color32::from_rgb(255, 138, 128),
            review: Color32::from_rgb(255, 204, 102),
            pass: Color32::from_rgb(120, 220, 140),
            muted: Color32::from_rgb(180, 180, 180),
        }
    } else {
        Palette {
            fail: Color32::from_rgb(176, 0, 32),
            review: Color32::from_rgb(138, 76, 0),
            pass: Color32::from_rgb(0, 110, 40),
            muted: Color32::from_rgb(85, 85, 85),
        }
    }
}

impl App {
    /// Starts checking every link on the scanned page (each address once) in the background.
    fn start_link_check(&mut self, ctx: &egui::Context) {
        self.cancel_link_check();
        let ScanState::Done(report) = &self.state else { return };
        let mut urls: Vec<String> = Vec::new();
        let mut index = Vec::with_capacity(report.links.len());
        for link in &report.links {
            if link.local_anchor.is_some() {
                index.push(None);
                continue;
            }
            let i = urls.iter().position(|u| *u == link.url).unwrap_or_else(|| {
                urls.push(link.url.clone());
                urls.len() - 1
            });
            index.push(Some(i));
        }
        self.link_results = vec![None; urls.len()];
        self.link_index = index;
        self.link_urls = urls.clone();
        if !urls.is_empty() {
            let ctx = ctx.clone();
            self.link_run = Some(links::start_with(urls, std::sync::Arc::new(move || ctx.request_repaint()), links::CheckedLink::check));
        }
    }

    fn cancel_link_check(&mut self) {
        if let Some(run) = self.link_run.take() {
            run.cancel();
        }
        self.link_urls.clear();
        self.link_results.clear();
        self.link_index.clear();
        self.selected_link = None;
    }

    fn link_check_started(&self) -> bool {
        self.link_run.is_some() || !self.link_index.is_empty()
    }

    fn is_scanning(&self) -> bool {
        matches!(self.state, ScanState::Scanning)
    }

    fn start_scan(&mut self, ctx: &egui::Context) {
        if self.is_scanning() {
            return;
        }
        let (tx, rx) = channel();
        let url = self.url.clone();
        let version = self.version;
        let level = self.level;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(scanner::scan(&url, version, level));
            ctx.request_repaint();
        });
        self.rx = Some(rx);
        self.state = ScanState::Scanning;
        self.copied = false;
        self.cancel_link_check();
        self.selected = None;
        self.code_view.clear();
        self.code_view.set_marks(Vec::new());
        // Start loading the live page in the preview while the scan runs.
        if let Ok(u) = scanner::normalize_url(&self.url) {
            self.preview.navigate(u);
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.rx {
            if let Ok(result) = rx.try_recv() {
                self.state = match result {
                    Ok(r) => ScanState::Done(r),
                    Err(e) => ScanState::Failed(e),
                };
                self.rx = None;
                self.publish_marks();
                if self.check_links {
                    self.start_link_check(ctx);
                }
            }
        }
        if let Some(run) = &self.link_run {
            let finished = loop {
                match run.rx.try_recv() {
                    Ok((i, result)) => {
                        if let Some(slot) = self.link_results.get_mut(i) {
                            *slot = Some(result);
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break false,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break true,
                }
            };
            if finished {
                self.link_run = None;
            }
        }
    }

    /// Selects a page link: highlights its element in the page preview and HTML source.
    fn select_link(&mut self, i: usize) {
        let ScanState::Done(report) = &self.state else { return };
        let Some(link) = report.links.get(i) else { return };
        let status = self.page_link_status(report, i);
        let name = if link.text.is_empty() { "(no link text)".to_string() } else { format!("“{}”", link.text) };
        let message = match &status {
            Some((_, detail)) => format!("Link {name} → {} ({detail})", link.url),
            None => format!("Link {name} → {}", link.url),
        };
        let finding = checks::Finding {
            severity: if matches!(status, Some((LinkBucket::Broken, _))) { Severity::Error } else { Severity::Warning },
            message,
            snippet: None,
            locator: Some(link.locator.clone()),
        };
        self.preview.highlight(&finding, "Link");
        self.code_view.select(&finding);
        self.selected = None;
        self.selected_link = Some(i);
        self.show_preview = true;
    }

    /// Status of page link `i`, or `None` while it can't be known (link checking is off and
    /// it isn't an in-page link, which the scan checks itself).
    fn page_link_status(&self, report: &Report, i: usize) -> Option<(LinkBucket, String)> {
        let link = report.links.get(i)?;
        if link.local_anchor.is_none() && !self.link_check_started() {
            return None;
        }
        let checked = self.link_index.get(i).copied().flatten().and_then(|u| self.link_results.get(u)).and_then(|r| r.as_ref());
        Some(link_status(link, checked))
    }

    /// One-line link-check summary on the WCAG tab. Returns true if "Show links" was clicked.
    fn links_summary(&self, ui: &mut egui::Ui) -> bool {
        let ScanState::Done(report) = &self.state else { return false };
        let pal = palette(ui.visuals().dark_mode);
        let buckets: Vec<LinkBucket> = (0..report.links.len()).filter_map(|i| self.page_link_status(report, i).map(|(b, _)| b)).collect();
        let count = |b: LinkBucket| buckets.iter().filter(|x| **x == b).count();
        let (broken, review, pending) = (count(LinkBucket::Broken), count(LinkBucket::Review), count(LinkBucket::Pending));
        let (text, color) = if report.links.is_empty() {
            ("Links: none on this page".to_string(), pal.muted)
        } else if pending > 0 {
            (format!("Links: checking… {pending} of {} left · {broken} broken so far", report.links.len()), pal.review)
        } else if broken + review > 0 {
            (format!("Links: {broken} broken · {review} to review (of {})", report.links.len()), if broken > 0 { pal.fail } else { pal.review })
        } else {
            (format!("Links: all {} working", report.links.len()), pal.pass)
        };
        let mut open = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new(text).strong().color(color));
            if !report.links.is_empty() && ui.small_button("Show links").clicked() {
                open = true;
            }
        });
        ui.add_space(4.0);
        open
    }

    /// The Links tab: every link on the page, with its status when known. Returns the link
    /// the user clicked (to show where it is on the page).
    fn links_tab(&mut self, ui: &mut egui::Ui) -> Option<usize> {
        let ScanState::Done(report) = &self.state else { return None };
        let pal = palette(ui.visuals().dark_mode);
        let statuses: Vec<Option<(LinkBucket, String)>> = (0..report.links.len()).map(|i| self.page_link_status(report, i)).collect();
        let checking = self.link_run.is_some();
        let started = self.link_check_started();
        let addresses = {
            let mut urls: Vec<&str> = report.links.iter().map(|l| l.url.as_str()).collect();
            urls.sort_unstable();
            urls.dedup();
            urls.len()
        };

        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("{} links on this page ({addresses} addresses)", report.links.len())).strong());
            if started && !checking {
                let count = |b: LinkBucket| statuses.iter().filter(|s| matches!(s, Some((x, _)) if *x == b)).count();
                ui.label(RichText::new(format!("✖ {} broken", count(LinkBucket::Broken))).color(pal.fail));
                ui.label(RichText::new(format!("⚠ {} to review", count(LinkBucket::Review))).color(pal.review));
                ui.label(RichText::new(format!("✔ {} working", count(LinkBucket::Ok) + count(LinkBucket::Redirected))).color(pal.pass));
            }
        });
        if checking {
            let total = statuses.len().max(1);
            let done = total - statuses.iter().filter(|s| matches!(s, Some((LinkBucket::Pending, _)))).count();
            ui.add(egui::ProgressBar::new(done as f32 / total as f32).text(format!("{done} of {total} links checked")));
        }
        ui.horizontal(|ui| {
            let label = ui.label("Filter:");
            ui.add(egui::TextEdit::singleline(&mut self.link_filter).hint_text("link text or address").desired_width(240.0))
                .labelled_by(label.id);
            if !self.link_filter.is_empty() && ui.small_button("✖").on_hover_text("Clear filter").clicked() {
                self.link_filter.clear();
            }
            if started {
                ui.checkbox(&mut self.links_only_problems, "Only problems");
            } else {
                ui.label(RichText::new("Tick “Check links” above to test whether each link works.").small().color(pal.muted));
            }
        });
        ui.label(RichText::new("Click a link to see where it is on the page and in the HTML source.").small().color(pal.muted));
        ui.add_space(4.0);

        if report.links.is_empty() {
            ui.label(RichText::new("This page has no links.").color(pal.muted));
            return None;
        }
        let needle = self.link_filter.trim().to_lowercase();
        let only_problems = started && self.links_only_problems;
        let shown: Vec<usize> = (0..report.links.len())
            .filter(|&i| {
                let l = &report.links[i];
                needle.is_empty() || l.text.to_lowercase().contains(&needle) || l.url.to_lowercase().contains(&needle)
            })
            .filter(|&i| !only_problems || matches!(statuses[i], Some((LinkBucket::Broken | LinkBucket::Review, _))))
            .collect();
        if shown.is_empty() {
            ui.label(RichText::new("No links match.").color(pal.muted));
            return None;
        }

        let mut clicked = None;
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 8.0;
        egui_extras::TableBuilder::new(ui)
            .id_salt("page-links-table")
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(egui_extras::Column::exact(40.0))
            .column(egui_extras::Column::initial(130.0).at_least(90.0).clip(true))
            .column(egui_extras::Column::initial(230.0).at_least(100.0).clip(true))
            .column(egui_extras::Column::remainder().at_least(150.0).clip(true))
            .header(row_h, |mut h| {
                for title in ["#", "Status", "Link text", "Address"] {
                    h.col(|ui| {
                        ui.strong(title);
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, shown.len(), |mut row| {
                    let i = shown[row.index()];
                    let link = &report.links[i];
                    row.set_selected(self.selected_link == Some(i));
                    row.col(|ui| {
                        ui.add(egui::Label::new(RichText::new((i + 1).to_string()).color(pal.muted)).selectable(false));
                    });
                    row.col(|ui| {
                        let (tag, color, detail) = match &statuses[i] {
                            Some((LinkBucket::Broken, d)) => ("✖ Broken", pal.fail, d.as_str()),
                            Some((LinkBucket::Review, d)) => ("⚠ Review", pal.review, d.as_str()),
                            Some((LinkBucket::Ok, d)) => ("✔ OK", pal.pass, d.as_str()),
                            Some((LinkBucket::Redirected, d)) => ("↪ Redirected", pal.pass, d.as_str()),
                            Some((LinkBucket::Pending, _)) => ("… Checking", pal.muted, ""),
                            None => ("—", pal.muted, "Not checked"),
                        };
                        let r = ui.add(egui::Label::new(RichText::new(tag).color(color)).selectable(false));
                        if !detail.is_empty() {
                            r.on_hover_text(detail);
                        }
                    });
                    row.col(|ui| {
                        let text = if link.text.is_empty() {
                            RichText::new("(no link text)").italics().color(pal.muted)
                        } else {
                            RichText::new(&link.text)
                        };
                        ui.add(egui::Label::new(text).truncate().selectable(false)).on_hover_text(&link.text);
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(RichText::new(&link.url).color(pal.muted)).truncate().selectable(false))
                            .on_hover_text(&link.url);
                    });
                    if row.response().clicked() {
                        clicked = Some(i);
                    }
                });
            });
        clicked
    }

    /// Sends every element finding to the page preview and the source view as marks.
    fn publish_marks(&mut self) {
        let ScanState::Done(report) = &self.state else { return };
        let mut page = Vec::new();
        let mut source = Vec::new();
        for (ri, rule) in report.results.iter().enumerate() {
            for (fi, f) in rule.findings.iter().enumerate() {
                let Some(loc) = &f.locator else { continue };
                page.push((ri, fi, f, rule.criterion));
                if let Some(range) = loc.source.clone() {
                    let sev = if f.severity == Severity::Error { "Error" } else { "Warning" };
                    source.push(Mark {
                        range,
                        severity: f.severity,
                        key: (ri, fi),
                        label: format!("{} · {sev}: {}", rule.criterion, f.message),
                    });
                }
            }
        }
        self.preview.set_markers(&page);
        self.code_view.set_marks(source);
    }

    /// Selects a finding and shows it everywhere. A click in the page preview reveals the
    /// element in the HTML source, and vice versa; the results list scrolls to the finding.
    fn select_finding(&mut self, ri: usize, fi: usize, origin: Origin) {
        let ScanState::Done(report) = &self.state else { return };
        let Some(rule) = report.results.get(ri) else { return };
        let Some(f) = rule.findings.get(fi) else { return };
        self.selected = Some((ri, fi));
        self.selected_link = None;
        self.preview.highlight(f, rule.criterion);
        self.code_view.select(f);
        self.show_preview = true;
        match origin {
            Origin::Page => self.side_tab = SideTab::Source,
            Origin::Source => self.side_tab = SideTab::Page,
            Origin::List => {}
        }
        if origin != Origin::List {
            self.results_tab = ResultsTab::Wcag;
            match rule.outcome() {
                Outcome::Fail => self.show_fail = true,
                Outcome::Review => self.show_review = true,
                Outcome::Pass => {}
            }
            self.reveal_frames = 10;
        }
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.heading("WCAG Compliance Scanner");
        ui.label("Scan one web page, or a list of pages, for a WCAG version and conformance level.");
        ui.add_space(6.0);
        let mut back = false;
        ui.horizontal(|ui| {
            ui.label("Scan:");
            if ui.selectable_label(self.scan_mode == ScanMode::Single, RichText::new("One page").strong()).clicked() {
                self.scan_mode = ScanMode::Single;
                self.viewing_batch = None;
            }
            let list_label = match self.batch.len() {
                0 => "List of pages".to_string(),
                n => format!("List of pages ({n})"),
            };
            if ui.selectable_label(self.scan_mode == ScanMode::List, RichText::new(list_label).strong()).clicked() {
                self.scan_mode = ScanMode::List;
                self.viewing_batch = None;
            }
            if let (ScanMode::List, Some(i)) = (self.scan_mode, self.viewing_batch) {
                ui.separator();
                if ui.button("← Back to list").clicked() {
                    back = true;
                }
                ui.label(RichText::new(format!("Page {} of {}", i + 1, self.batch.len())).small());
            }
        });
        if back {
            self.viewing_batch = None;
        }
        ui.add_space(4.0);
        let single = self.scan_mode == ScanMode::Single;

        let mut trigger = false;
        ui.horizontal(|ui| {
            let scanning = self.is_scanning();
            if single {
                let label = ui.label("URL:");
                let width = (ui.available_width() - 500.0).max(160.0);
                let edit = ui
                    .add_enabled(
                        !scanning,
                        egui::TextEdit::singleline(&mut self.url)
                            .hint_text("https://example.com")
                            .desired_width(width),
                    )
                    .labelled_by(label.id);
                if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    trigger = true;
                }
            }

            let ver_label = ui.label("WCAG:");
            egui::ComboBox::from_id_salt("wcag-version")
                .selected_text(self.version.to_string())
                .width(60.0)
                .show_ui(ui, |ui| {
                    for v in Version::ALL {
                        let desc = match v {
                            Version::V2_0 => "2.0 (2008, Section 508)",
                            Version::V2_1 => "2.1 (2018, ADA Title II / EN 301 549)",
                            Version::V2_2 => "2.2 (2023, latest)",
                        };
                        ui.selectable_value(&mut self.version, v, desc);
                    }
                })
                .response
                .labelled_by(ver_label.id);

            let lvl_label = ui.label("Level:");
            egui::ComboBox::from_id_salt("wcag-level")
                .selected_text(self.level.to_string())
                .width(50.0)
                .show_ui(ui, |ui| {
                    for l in Level::ALL {
                        let desc = match l {
                            Level::A => "A — minimum",
                            Level::AA => "AA — standard (legal target)",
                            Level::AAA => "AAA — enhanced",
                        };
                        ui.selectable_value(&mut self.level, l, desc);
                    }
                })
                .response
                .labelled_by(lvl_label.id);

            if single {
                let scan_btn = ui.add_enabled(
                    !scanning && !self.url.trim().is_empty(),
                    egui::Button::new(RichText::new("  Scan  ").strong()),
                );
                if scan_btn.clicked() {
                    trigger = true;
                }
            }
            ui.checkbox(&mut self.show_preview, "Preview")
                .on_hover_text("Show the page preview and HTML source panel, highlighting the selected finding");
            if ui
                .checkbox(&mut self.check_links, "Check links")
                .on_hover_text("Also check every link on the page for broken links (in the background, after the scan)")
                .changed()
            {
                self.check_links_changed = true;
                if self.check_links {
                    if matches!(self.state, ScanState::Done(_)) && !self.link_check_started() {
                        self.start_link_check(ui.ctx());
                    }
                } else {
                    self.cancel_link_check();
                }
            }
            if scanning {
                ui.spinner();
                ui.label("Scanning…");
            }
        });
        if trigger && !self.url.trim().is_empty() {
            self.start_scan(ui.ctx());
        }
        ui.add_space(6.0);
    }

    fn summary(&mut self, ui: &mut egui::Ui) {
        let pal = palette(ui.visuals().dark_mode);
        let ScanState::Done(report) = &self.state else { return };
        ui.separator();
        egui::Grid::new("summary").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
            ui.label(RichText::new("Page").strong());
            ui.label(report.page_title.as_deref().unwrap_or("(no title)"));
            ui.end_row();
            ui.label(RichText::new("URL").strong());
            ui.hyperlink_to(&report.final_url, &report.final_url);
            ui.end_row();
            ui.label(RichText::new("Scanned").strong());
            ui.label(format!(
                "WCAG {} Level {} · {} criteria checks · {} · {:.1} KB in {:.2}s",
                report.version,
                report.level,
                report.results.len(),
                report.status.map(|s| format!("HTTP {s}")).unwrap_or_else(|| "local file".into()),
                report.bytes as f64 / 1024.0,
                report.elapsed.as_secs_f64()
            ));
            ui.end_row();
        });
        ui.add_space(4.0);

        let fails = report.outcomes(Outcome::Fail);
        let reviews = report.outcomes(Outcome::Review);
        let passes = report.outcomes(Outcome::Pass);
        let verdict = if fails == 0 {
            RichText::new(format!(
                "No automated failures detected for WCAG {} Level {}{}",
                report.version,
                report.level,
                if reviews > 0 { " — some items need manual review" } else { "" }
            ))
            .color(pal.pass)
        } else {
            RichText::new(format!("Does not conform to WCAG {} Level {}", report.version, report.level)).color(pal.fail)
        };
        ui.label(verdict.strong().size(16.0));

        let text = report.to_text();
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.show_fail, RichText::new(format!("Failed ({fails})")).color(pal.fail));
            ui.checkbox(&mut self.show_review, RichText::new(format!("Needs review ({reviews})")).color(pal.review));
            ui.checkbox(&mut self.show_pass, RichText::new(format!("Passed ({passes})")).color(pal.pass));
            ui.separator();
            ui.label(format!(
                "{} errors · {} warnings",
                report.total(Severity::Error),
                report.total(Severity::Warning)
            ));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(if self.copied { "Copied ✔" } else { "Copy report" }).clicked() {
                    ui.ctx().copy_text(text);
                    self.copied = true;
                }
            });
        });
        ui.add_space(4.0);
    }

    /// Draws the results list. Returns the finding the user clicked, if any.
    fn results(&self, ui: &mut egui::Ui) -> Option<(usize, usize)> {
        let mut clicked = None;
        let pal = palette(ui.visuals().dark_mode);
        match &self.state {
            ScanState::Idle => {
                ui.add_space(20.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("No scan yet.").size(16.0));
                    ui.label(RichText::new("Results will appear here.").color(pal.muted));
                });
            }
            ScanState::Scanning => {
                ui.add_space(20.0);
                ui.vertical_centered(|ui| {
                    ui.spinner();
                    ui.label("Fetching and analyzing the page…");
                });
            }
            ScanState::Failed(e) => {
                ui.add_space(12.0);
                ui.label(RichText::new("Scan failed").strong().color(pal.fail).size(16.0));
                ui.label(e);
            }
            ScanState::Done(report) => {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    let mut sorted: Vec<_> = report.results.iter().enumerate().collect();
                    sorted.sort_by_key(|(_, r)| match r.outcome() {
                        Outcome::Fail => 0,
                        Outcome::Review => 1,
                        Outcome::Pass => 2,
                    });
                    let mut shown = 0;
                    for (i, r) in sorted {
                        let outcome = r.outcome();
                        let visible = match outcome {
                            Outcome::Fail => self.show_fail,
                            Outcome::Review => self.show_review,
                            Outcome::Pass => self.show_pass,
                        };
                        if !visible {
                            continue;
                        }
                        shown += 1;
                        let (tag, color) = match outcome {
                            Outcome::Fail => ("FAIL", pal.fail),
                            Outcome::Review => ("REVIEW", pal.review),
                            Outcome::Pass => ("PASS", pal.pass),
                        };
                        let new_in = if r.since > Version::V2_0 { format!(", new in {}", r.since) } else { String::new() };
                        let mut header = format!("{tag}  ·  {} (Level {}{new_in})  {}", r.criterion, r.level, r.title);
                        if !r.findings.is_empty() {
                            header.push_str(&format!(
                                "  —  {} error(s), {} warning(s)",
                                r.count(Severity::Error),
                                r.count(Severity::Warning)
                            ));
                        }
                        let header = RichText::new(header).color(color).strong();
                        if r.findings.is_empty() {
                            ui.label(header);
                            continue;
                        }
                        let reveal = self.reveal_frames > 0 && self.selected.is_some_and(|(ri, _)| ri == i);
                        egui::CollapsingHeader::new(header)
                            .id_salt(("rule", i))
                            .default_open(outcome == Outcome::Fail)
                            .open(reveal.then_some(true))
                            .show(ui, |ui| {
                                const MAX: usize = 100;
                                for (fi, f) in r.findings.iter().enumerate().take(MAX) {
                                    let is_selected = self.selected == Some((i, fi));
                                    let row = finding_row(ui, &pal, f, is_selected);
                                    if is_selected && reveal {
                                        row.scroll_to_me(Some(egui::Align::Center));
                                    }
                                    if row.clicked() {
                                        clicked = Some((i, fi));
                                    }
                                    ui.add_space(2.0);
                                }
                                if r.findings.len() > MAX {
                                    ui.label(
                                        RichText::new(format!("…and {} more (see Copy report)", r.findings.len() - MAX))
                                            .italics(),
                                    );
                                }
                            });
                    }
                    if shown == 0 {
                        ui.label(RichText::new("Nothing to show with the current filters.").color(pal.muted));
                    }
                    ui.add_space(12.0);
                    ui.separator();
                    ui.label(
                        RichText::new(
                            "Note: this tool analyzes the HTML returned by the server. Content rendered by JavaScript \
                             and colours set in external stylesheets are not evaluated. Automated testing finds only \
                             a portion of accessibility issues — manual review with a keyboard and screen reader is \
                             still required to confirm conformance.",
                        )
                        .small()
                        .color(pal.muted),
                    );
                });
            }
        }
        clicked
    }

    /// The right-hand panel: tabs for the live page preview and the HTML source.
    fn side_panel(&mut self, ui: &mut egui::Ui, frame: &eframe::Frame) {
        let has_source = matches!(self.state, ScanState::Done(_));
        // Keep the user's tab choice across rescans; just show the page until source exists.
        let tab = if has_source { self.side_tab } else { SideTab::Page };
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.selectable_label(tab == SideTab::Page, RichText::new("Page preview").strong()).clicked() {
                self.side_tab = SideTab::Page;
            }
            ui.add_enabled_ui(has_source, |ui| {
                if ui
                    .selectable_label(tab == SideTab::Source, RichText::new("HTML source").strong())
                    .on_disabled_hover_text("Available after a scan completes")
                    .clicked()
                {
                    self.side_tab = SideTab::Source;
                }
            });
            if tab == SideTab::Page && self.preview.is_loading() {
                ui.spinner();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if tab == SideTab::Page && ui.button("Reload").clicked() {
                    self.preview.reload();
                }
                if ui
                    .checkbox(&mut self.show_all_marks, "Show all issues")
                    .on_hover_text("Mark every flagged element in the page preview and HTML source; click a mark to jump to it in the other view")
                    .changed()
                {
                    self.preview.set_markers_visible(self.show_all_marks);
                    self.code_view.set_marks_visible(self.show_all_marks);
                }
                if self.selected.is_some() && ui.button("Clear highlight").clicked() {
                    self.selected = None;
                    self.preview.clear_highlight();
                    self.code_view.clear();
                }
            });
        });
        ui.separator();
        match (tab, &self.state) {
            (SideTab::Source, ScanState::Done(report)) => {
                self.preview.hide();
                if let Some((ri, fi)) = self.code_view.show(ui, &report.source) {
                    self.select_finding(ri, fi, Origin::Source);
                }
            }
            _ => self.preview_panel(ui, frame),
        }
    }

    fn preview_panel(&mut self, ui: &mut egui::Ui, frame: &eframe::Frame) {
        let pal = palette(ui.visuals().dark_mode);
        if let Some(u) = self.preview.url() {
            ui.add(egui::Label::new(RichText::new(u).small().color(pal.muted)).truncate());
        }
        if let Some(e) = self.preview.error() {
            ui.label(RichText::new(format!("Preview unavailable: {e}")).color(pal.fail));
            self.preview.hide();
            return;
        }
        ui.add_space(4.0);
        let rect = ui.available_rect_before_wrap();
        ui.allocate_rect(rect, egui::Sense::hover());
        ui.painter().rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
        // The webview is a native view drawn above egui, so hide it while a dropdown
        // is open or the dropdown would be covered.
        if egui::Popup::is_any_open(ui.ctx()) {
            self.preview.hide();
        } else {
            self.preview.place(ui.ctx(), frame, rect);
        }
    }
}

/// A clickable, keyboard-focusable row for one finding.
fn finding_row(ui: &mut egui::Ui, pal: &Palette, f: &checks::Finding, selected: bool) -> egui::Response {
    let (sev, c) = match f.severity {
        Severity::Error => ("Error", pal.fail),
        Severity::Warning => ("Warning", pal.review),
    };
    let visuals = ui.visuals();
    let stroke = if selected {
        egui::Stroke::new(2.0, visuals.selection.stroke.color)
    } else {
        egui::Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color)
    };
    let fill = if selected { visuals.faint_bg_color } else { Color32::TRANSPARENT };
    let inner = egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(8, 5))
        .corner_radius(4.0)
        .stroke(stroke)
        .fill(fill)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                if selected {
                    ui.label(RichText::new("▶").color(c));
                }
                ui.label(RichText::new(format!("{sev}:")).color(c).strong());
                ui.label(&f.message);
            });
            match &f.snippet {
                Some(sn) => {
                    ui.add(egui::Label::new(RichText::new(sn).monospace().color(pal.muted)).wrap());
                }
                None => {
                    ui.label(RichText::new("Page-level issue").small().italics().color(pal.muted));
                }
            }
        });
    let label = format!("{sev}: {}", f.message);
    let response = inner
        .response
        .interact(egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("Show in page preview and HTML source");
    response.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, selected, &label));
    if response.has_focus() {
        ui.painter().rect_stroke(
            response.rect.expand(1.0),
            4.0,
            ui.visuals().selection.stroke,
            egui::StrokeKind::Outside,
        );
    }
    response
}

impl App {
    fn save_sortsite_settings(&self, storage: &mut dyn eframe::Storage) {
        storage.set_string(SORTSITE_REOPEN_KEY, self.sortsite.reopen_last().to_string());
        let last = self.sortsite.last_file().map(|p| p.display().to_string()).unwrap_or_default();
        storage.set_string(SORTSITE_LAST_FILE_KEY, last);
    }
}

/// One entry in the left-hand navigation: a tab with a title and a short description.
fn nav_tab(ui: &mut egui::Ui, selected: bool, title: &str, subtitle: &str) -> egui::Response {
    // Lay the text out first, wrapped to the box width, so the box grows to fit it.
    const PAD_X: f32 = 14.0;
    const PAD_Y: f32 = 9.0;
    const GAP: f32 = 4.0;
    let width = ui.available_width();
    let wrap = (width - PAD_X - 10.0).max(40.0);
    let (title_galley, subtitle_galley) = ui.fonts_mut(|f| {
        (
            f.layout(title.to_string(), egui::FontId::proportional(15.0), egui::Color32::PLACEHOLDER, wrap),
            f.layout(subtitle.to_string(), egui::FontId::proportional(12.0), egui::Color32::PLACEHOLDER, wrap),
        )
    });
    let height = (PAD_Y + title_galley.size().y + GAP + subtitle_galley.size().y + PAD_Y).max(52.0);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click());
    let visuals = ui.style().interact_selectable(&resp, selected);
    let painter = ui.painter();
    painter.rect(rect, 6.0, visuals.weak_bg_fill, visuals.bg_stroke, egui::StrokeKind::Inside);
    if selected {
        let bar = egui::Rect::from_min_size(rect.min, egui::vec2(4.0, rect.height()));
        painter.rect_filled(bar, 2.0, ui.visuals().selection.stroke.color);
    }
    let text = visuals.text_color();
    let title_pos = rect.left_top() + egui::vec2(PAD_X, PAD_Y);
    let subtitle_pos = title_pos + egui::vec2(0.0, title_galley.size().y + GAP);
    painter.galley(title_pos, title_galley, text);
    painter.galley(subtitle_pos, subtitle_galley, text);
    if resp.has_focus() {
        painter.rect_stroke(rect.expand(1.0), 6.0, ui.visuals().selection.stroke, egui::StrokeKind::Outside);
    }
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Tab, true, selected, format!("{title}. {subtitle}")));
    resp
}

impl App {
    fn nav(&mut self, ui: &mut egui::Ui) {
        let pal = palette(ui.visuals().dark_mode);
        ui.add_space(12.0);
        ui.label(RichText::new("ADA Checker").size(20.0).strong());
        ui.label(RichText::new("Web accessibility tools").small().color(pal.muted));
        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);
        for (tool, title, subtitle) in [
            (Tool::Sortsite, "Sortsite Eval", "Review a SortSite Excel report"),
            (Tool::Accessibility, "WCAG Scanner", "Scan pages for WCAG issues"),
            (Tool::Links, "Link Checker", "Find broken links in a list"),
        ] {
            if nav_tab(ui, self.tool == tool, title, subtitle).clicked() {
                self.tool = tool;
            }
            ui.add_space(4.0);
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(8.0);
            ui.label(RichText::new(concat!("Version ", env!("CARGO_PKG_VERSION"))).small().color(pal.muted));
        });
    }

    /// The WCAG Scanner tool (URL scanner, results, preview and source).
    /// Shows list page `i`'s full results in the one-page view (preview, source, links).
    fn open_batch_report(&mut self, i: usize, ctx: &egui::Context) {
        let Some(report) = self.batch.report(i).cloned() else { return };
        self.viewing_batch = Some(i);
        self.url = report.final_url.clone();
        self.rx = None;
        self.cancel_link_check();
        self.selected = None;
        self.code_view.clear();
        self.results_tab = ResultsTab::Wcag;
        let url = report.final_url.clone();
        self.state = ScanState::Done(report);
        self.publish_marks();
        if let Ok(u) = scanner::normalize_url(&url) {
            self.preview.navigate(u);
        }
        if self.check_links {
            self.start_link_check(ctx);
        }
    }

    fn accessibility_ui(&mut self, ui: &mut egui::Ui, frame: &eframe::Frame) {
        if self.scan_mode == ScanMode::List && self.viewing_batch.is_none() {
            // The list view: the pasted pages and a summary row per page.
            egui::Panel::top("controls").show(ui, |ui| self.controls(ui));
            self.preview.hide();
            match self.batch.ui(ui, self.version, self.level) {
                Some(batch::Action::Start) => self.batch.start(self.version, self.level, ui.ctx()),
                Some(batch::Action::Open(i)) => self.open_batch_report(i, ui.ctx()),
                None => {}
            }
            return;
        }
        egui::Panel::top("controls").show(ui, |ui| {
            self.controls(ui);
            self.summary(ui);
        });

        if self.show_preview && (self.preview.has_content() || matches!(self.state, ScanState::Done(_))) {
            let default_width = ui.available_width() * 0.55;
            egui::Panel::right("preview")
                .resizable(true)
                .default_size(default_width)
                .min_size(360.0)
                .show(ui, |ui| self.side_panel(ui, frame));
        } else {
            self.preview.hide();
        }

        let (clicked, link_clicked) = egui::CentralPanel::default()
            .show(ui, |ui| {
                let ScanState::Done(report) = &self.state else { return (self.results(ui), None) };
                let link_count = report.links.len();
                ui.horizontal(|ui| {
                    if ui.selectable_label(self.results_tab == ResultsTab::Wcag, RichText::new("WCAG results").strong()).clicked() {
                        self.results_tab = ResultsTab::Wcag;
                    }
                    if ui
                        .selectable_label(self.results_tab == ResultsTab::Links, RichText::new(format!("Links ({link_count})")).strong())
                        .on_hover_text("Every link on the page — click one to see where it is")
                        .clicked()
                    {
                        self.results_tab = ResultsTab::Links;
                    }
                });
                ui.separator();
                match self.results_tab {
                    ResultsTab::Wcag => {
                        if (self.check_links || self.link_check_started()) && self.links_summary(ui) {
                            self.results_tab = ResultsTab::Links;
                        }
                        (self.results(ui), None)
                    }
                    ResultsTab::Links => (None, self.links_tab(ui)),
                }
            })
            .inner;
        if let Some(i) = link_clicked {
            self.select_link(i);
        }
        if self.debug_select_link.is_some() && matches!(self.state, ScanState::Done(_)) && self.link_run.is_none() && !self.preview.is_loading() {
            let i = self.debug_select_link.take().expect("checked");
            self.select_link(i);
        }
        if let Some((ri, fi)) = clicked {
            self.select_finding(ri, fi, Origin::List);
        }
        // Debug builds only: ADA_SELECT_FIRST=1 selects the first element finding, as if clicked
        // in the results list (or in the HTML source when ADA_SELECT_ORIGIN=source).
        if let (true, ScanState::Done(report)) = (self.auto_select, &self.state) {
            self.auto_select = false;
            // ADA_SELECT_CRITERION=4.1.1 restricts the pick to one criterion.
            let criterion = std::env::var("ADA_SELECT_CRITERION").ok();
            let first = report.results.iter().enumerate().find_map(|(ri, r)| {
                if criterion.as_deref().is_some_and(|c| !r.criterion.starts_with(c)) {
                    return None;
                }
                r.findings.iter().position(|f| f.locator.is_some()).map(|fi| (ri, fi))
            });
            let origin = match std::env::var("ADA_SELECT_ORIGIN").as_deref() {
                Ok("source") => Origin::Source,
                _ => Origin::List,
            };
            if let Some((ri, fi)) = first {
                self.select_finding(ri, fi, origin);
            }
        }
        // Badge clicks inside the page preview (delivered over IPC).
        if let Some((ri, fi)) = self.preview.take_clicks().pop() {
            self.select_finding(ri, fi, Origin::Page);
        }
        if self.reveal_frames > 0 {
            self.reveal_frames -= 1;
            ui.ctx().request_repaint();
        }
        if self.debug_click_badge && matches!(self.state, ScanState::Done(_)) && !self.preview.is_loading() {
            self.debug_click_badge = false;
            self.preview.run_now_if_ready(
                "setTimeout(function () { var b = document.querySelector('button.__ada_checker_marker'); if (b) b.click(); }, 1500);",
            );
        }
    }}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string(DETAILS_SIDE_KEY, self.sortsite.details_side().as_str().to_string());
        self.save_sortsite_settings(storage);
        // "Check links" is saved only when the user changes it (see `ui`), so a debug-build
        // override (ADA_CHECK_LINKS) never ends up in the user's settings.
    }

    /// Only our own settings are saved, not egui's internal UI state.
    fn persist_egui_memory(&self) -> bool {
        false
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        // Keep polling the scans so they complete even while another tool is shown.
        self.poll(ui.ctx());
        self.batch.poll();
        if self.batch.take_autostart() {
            self.batch.start(self.version, self.level, ui.ctx());
        }
        if let Some(i) = self.debug_open_batch {
            if self.batch.report(i).is_some() {
                self.debug_open_batch = None;
                self.open_batch_report(i, ui.ctx());
            }
        }
        // "ADA Check" in Sortsite Eval: scan that page and show the WCAG Scanner.
        if let Some(url) = self.sortsite.take_ada_check_request() {
            self.scan_mode = ScanMode::Single;
            self.viewing_batch = None;
            self.url = url;
            self.tool = Tool::Accessibility;
            self.scan_on_start = true;
        }
        // Start a requested scan once any scan already running has finished.
        if self.scan_on_start && !self.is_scanning() {
            self.scan_on_start = false;
            self.start_scan(ui.ctx());
        }
        egui::Panel::left("nav").resizable(false).exact_size(210.0).show(ui, |ui| self.nav(ui));
        match self.tool {
            Tool::Accessibility => self.accessibility_ui(ui, frame),
            Tool::Links => {
                // The page preview is a native view drawn over egui; keep it out of the way.
                self.preview.hide();
                self.link_checker.ui(ui);
            }
            Tool::Sortsite => {
                self.preview.hide();
                self.sortsite.ui(ui);
            }
        }
        // Finish loading a spreadsheet even while another tool is shown.
        self.sortsite.frame(ui.ctx());
        // Save a new dock-side choice immediately (not just on quit), so it survives a crash.
        if self.sortsite.take_details_side_changed() {
            if let Some(storage) = frame.storage_mut() {
                storage.set_string(DETAILS_SIDE_KEY, self.sortsite.details_side().as_str().to_string());
                storage.flush();
            }
        }
        if std::mem::take(&mut self.check_links_changed) {
            if let Some(storage) = frame.storage_mut() {
                storage.set_string(CHECK_LINKS_KEY, self.check_links.to_string());
                storage.flush();
            }
        }
        if self.sortsite.take_settings_changed() {
            if let Some(storage) = frame.storage_mut() {
                self.save_sortsite_settings(storage);
                storage.flush();
            }
        }
    }
}
