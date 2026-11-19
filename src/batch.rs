//! WCAG Scanner "List of pages" mode: scans a pasted list of URLs in the background and
//! shows a summary row per page. Any finished page can be opened in the normal one-page view.

use crate::checks::{Level, Outcome, Severity, Version};
use crate::links::{self, LinkRun, Parsed};
use crate::scanner::{self, Report};
use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Pages scanned at once: enough to be quick, few enough not to hammer one site.
pub const WORKERS: usize = 4;

pub type ScanResult = Result<Report, String>;

/// How a page's scan came out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageResult {
    Queued,
    Fails,
    /// No automated failures, but some items need manual review.
    Review,
    Passes,
    /// The page couldn't be fetched or read.
    Error,
}

pub fn page_result(row: &Option<ScanResult>) -> PageResult {
    match row {
        None => PageResult::Queued,
        Some(Err(_)) => PageResult::Error,
        Some(Ok(r)) if r.outcomes(Outcome::Fail) > 0 => PageResult::Fails,
        Some(Ok(r)) if r.outcomes(Outcome::Review) > 0 => PageResult::Review,
        Some(Ok(_)) => PageResult::Passes,
    }
}

#[derive(Default)]
pub struct BatchScan {
    input: String,
    parsed: Parsed,
    /// The pages of the current/last run, with results as they arrive.
    pub rows: Vec<(String, Option<ScanResult>)>,
    run: Option<LinkRun<ScanResult>>,
    started: Option<Instant>,
    took: Option<Duration>,
    stopping: bool,
    /// The WCAG version and level of the current/last run.
    scanned_as: Option<(Version, Level)>,
    only_problems: bool,
    copied: bool,
    autostart: bool,
}

struct Colors {
    fail: Color32,
    review: Color32,
    pass: Color32,
    muted: Color32,
}

fn colors(dark: bool) -> Colors {
    if dark {
        Colors {
            fail: Color32::from_rgb(255, 138, 128),
            review: Color32::from_rgb(255, 204, 102),
            pass: Color32::from_rgb(120, 220, 140),
            muted: Color32::from_rgb(170, 170, 170),
        }
    } else {
        Colors {
            fail: Color32::from_rgb(176, 0, 32),
            review: Color32::from_rgb(138, 76, 0),
            pass: Color32::from_rgb(0, 110, 40),
            muted: Color32::from_rgb(85, 85, 85),
        }
    }
}

/// What the user did in the list view.
pub enum Action {
    /// Start scanning the pasted list.
    Start,
    /// Open page `i`'s full results in the one-page view.
    Open(usize),
}

impl BatchScan {
    pub fn is_running(&self) -> bool {
        self.run.is_some()
    }

    /// Replaces the pasted text; optionally starts scanning on the next frame.
    pub fn load(&mut self, text: String, start: bool) {
        self.parsed = links::parse_links(&text);
        self.input = text;
        self.autostart = start;
    }

    pub fn take_autostart(&mut self) -> bool {
        std::mem::take(&mut self.autostart)
    }

    pub fn start(&mut self, version: Version, level: Level, ctx: &egui::Context) {
        if self.is_running() || self.parsed.links.is_empty() {
            return;
        }
        let urls = self.parsed.links.clone();
        self.rows = urls.iter().map(|u| (u.clone(), None)).collect();
        let ctx = ctx.clone();
        self.run = Some(links::start_pool(urls, WORKERS, Arc::new(move || ctx.request_repaint()), move |_client, url| {
            scanner::scan(url, version, level)
        }));
        self.started = Some(Instant::now());
        self.took = None;
        self.stopping = false;
        self.scanned_as = Some((version, level));
        self.copied = false;
    }

    /// Collects finished scans; called every frame whichever view is shown.
    pub fn poll(&mut self) {
        let Some(run) = &self.run else { return };
        let finished = loop {
            match run.rx.try_recv() {
                Ok((i, result)) => {
                    if let Some(row) = self.rows.get_mut(i) {
                        row.1 = Some(result);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break false,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break true,
            }
        };
        if finished {
            self.took = self.started.map(|s| s.elapsed());
            self.run = None;
        }
    }

    fn stop(&mut self) {
        if let Some(run) = &self.run {
            run.cancel();
            self.stopping = true;
        }
    }

    pub fn report(&self, i: usize) -> Option<&Report> {
        self.rows.get(i).and_then(|(_, r)| r.as_ref()).and_then(|r| r.as_ref().ok())
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// CSV of the results, one line per page.
    pub fn to_csv(&self) -> String {
        let mut out = String::from("url,result,failed_criteria,needs_review,errors,warnings,page_title,final_url,wcag,error\n");
        for (url, row) in &self.rows {
            let result = match page_result(row) {
                PageResult::Queued => "not scanned",
                PageResult::Fails => "fails",
                PageResult::Review => "needs review",
                PageResult::Passes => "no failures",
                PageResult::Error => "error",
            };
            let line = match row {
                Some(Ok(r)) => [
                    links::csv(url),
                    result.into(),
                    r.outcomes(Outcome::Fail).to_string(),
                    r.outcomes(Outcome::Review).to_string(),
                    r.total(Severity::Error).to_string(),
                    r.total(Severity::Warning).to_string(),
                    links::csv(r.page_title.as_deref().unwrap_or("")),
                    links::csv(&r.final_url),
                    format!("{} {}", r.version, r.level),
                    String::new(),
                ]
                .join(","),
                Some(Err(e)) => format!("{},{result},,,,,,,,{}", links::csv(url), links::csv(e)),
                None => format!("{},{result},,,,,,,,", links::csv(url)),
            };
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    /// The list view: the URL list on the left, results on the right.
    pub fn ui(&mut self, ui: &mut egui::Ui, version: Version, level: Level) -> Option<Action> {
        let mut action = None;
        egui::Panel::left("batch-input")
            .resizable(true)
            .default_size(380.0)
            .min_size(260.0)
            .show(ui, |ui| {
                if self.input_pane(ui, version, level) {
                    action = Some(Action::Start);
                }
            });
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(i) = self.results_pane(ui) {
                action = Some(Action::Open(i));
            }
        });
        action
    }

    /// Returns true if "Scan N pages" was pressed.
    fn input_pane(&mut self, ui: &mut egui::Ui, version: Version, level: Level) -> bool {
        let c = colors(ui.visuals().dark_mode);
        let mut start = false;
        ui.add_space(6.0);
        ui.heading("Pages to scan");
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let n = self.parsed.links.len();
            let label = if n == 1 { "Scan 1 page".to_string() } else { format!("Scan {n} pages") };
            if ui
                .add_enabled(!self.is_running() && n > 0, egui::Button::new(RichText::new(label).strong()))
                .on_hover_text(format!("Scan each page for WCAG {version} Level {level} ({WORKERS} at a time)"))
                .on_disabled_hover_text(if n == 0 { "Paste some page addresses first" } else { "A scan is already running" })
                .clicked()
            {
                start = true;
            }
            if self.is_running() {
                if ui.add_enabled(!self.stopping, egui::Button::new("Stop")).clicked() {
                    self.stop();
                }
                ui.spinner();
            } else if ui.add_enabled(!self.input.is_empty(), egui::Button::new("Clear")).clicked() {
                self.input.clear();
                self.parsed = Parsed::default();
            }
        });
        ui.add_space(4.0);
        let mut summary = match self.parsed.links.len() {
            0 => "No page addresses found yet.".to_string(),
            1 => "1 page".to_string(),
            n => format!("{n} pages"),
        };
        if self.parsed.duplicates > 0 {
            summary.push_str(&format!(" · {} duplicate(s) ignored", self.parsed.duplicates));
        }
        let summary_label = ui.label(RichText::new(summary).small().color(c.muted));
        ui.add_space(4.0);
        egui::ScrollArea::vertical().id_salt("batch-input-scroll").auto_shrink([false, false]).show(ui, |ui| {
            let edit = ui
                .add_sized(
                    ui.available_size(),
                    egui::TextEdit::multiline(&mut self.input)
                        .font(egui::TextStyle::Monospace)
                        .hint_text("Paste page addresses here, one per line.\n\nhttps://example.com/\nexample.org/about\n\nAddresses inside text, Markdown or HTML are found too.\nLines starting with # are ignored.")
                        .desired_width(f32::INFINITY),
                )
                .labelled_by(summary_label.id);
            if edit.changed() {
                self.parsed = links::parse_links(&self.input);
            }
        });
        start
    }

    /// Returns the page whose row was clicked.
    fn results_pane(&mut self, ui: &mut egui::Ui) -> Option<usize> {
        let c = colors(ui.visuals().dark_mode);
        ui.add_space(6.0);
        ui.heading("Results");
        if self.rows.is_empty() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No pages scanned yet.").size(16.0));
                ui.label(RichText::new("Paste page addresses on the left and press Scan.").color(c.muted));
            });
            return None;
        }
        if let Some((v, l)) = self.scanned_as {
            ui.label(RichText::new(format!("WCAG {v} Level {l}")).small().color(c.muted));
        }
        ui.add_space(4.0);

        let results: Vec<PageResult> = self.rows.iter().map(|(_, r)| page_result(r)).collect();
        let count = |p: PageResult| results.iter().filter(|r| **r == p).count();
        let total = self.rows.len();
        let done = total - count(PageResult::Queued);
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("✖ Fails {}", count(PageResult::Fails))).color(c.fail).strong());
            ui.add_space(8.0);
            ui.label(RichText::new(format!("⚠ Needs review {}", count(PageResult::Review))).color(c.review).strong());
            ui.add_space(8.0);
            ui.label(RichText::new(format!("✔ No failures {}", count(PageResult::Passes))).color(c.pass).strong());
            ui.add_space(8.0);
            ui.label(RichText::new(format!("✖ Couldn't scan {}", count(PageResult::Error))).color(c.fail).strong());
        });
        if self.is_running() {
            let text = if self.stopping { format!("Stopping… {done} of {total}") } else { format!("{done} of {total} pages scanned") };
            ui.add(egui::ProgressBar::new(done as f32 / total.max(1) as f32).text(text));
        } else if let Some(t) = self.took {
            let note = if done < total { format!("Stopped after {done} of {total} pages") } else { format!("Scanned {total} page(s)") };
            ui.label(RichText::new(format!("{note} in {:.1}s", t.as_secs_f64())).small().color(c.muted));
        }
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.only_problems, "Only pages with problems");
            ui.label(RichText::new("Click a page to open its full results.").small().color(c.muted));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button(if self.copied { "Copied ✔" } else { "Copy as CSV" })
                    .on_hover_text("Copy one line per page, for a spreadsheet")
                    .clicked()
                {
                    ui.ctx().copy_text(self.to_csv());
                    self.copied = true;
                }
            });
        });
        ui.add_space(4.0);

        let shown: Vec<usize> = (0..total)
            .filter(|&i| !self.only_problems || matches!(results[i], PageResult::Fails | PageResult::Review | PageResult::Error))
            .collect();
        if shown.is_empty() {
            ui.label(RichText::new("No pages with problems.").color(c.pass));
            return None;
        }

        let mut clicked = None;
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 8.0;
        TableBuilder::new(ui)
            .id_salt("batch-results")
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(36.0))
            .column(Column::initial(130.0).at_least(100.0))
            .column(Column::initial(70.0).at_least(56.0))
            .column(Column::initial(70.0).at_least(56.0))
            .column(Column::initial(70.0).at_least(56.0))
            .column(Column::initial(220.0).at_least(100.0).clip(true))
            .column(Column::remainder().at_least(160.0).clip(true))
            .header(row_h + 2.0, |mut h| {
                for (title, hover) in [
                    ("#", ""),
                    ("Result", ""),
                    ("Failed", "WCAG criteria failed"),
                    ("Errors", "Error findings"),
                    ("Warnings", "Warning findings (need review)"),
                    ("Page title", ""),
                    ("Address", ""),
                ] {
                    h.col(|ui| {
                        let r = ui.strong(title);
                        if !hover.is_empty() {
                            r.on_hover_text(hover);
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, shown.len(), |mut row| {
                    let i = shown[row.index()];
                    let (url, result) = &self.rows[i];
                    let (tag, color) = match results[i] {
                        PageResult::Queued => (if self.is_running() { "… Scanning" } else { "Not scanned" }, c.muted),
                        PageResult::Fails => ("✖ Fails", c.fail),
                        PageResult::Review => ("⚠ Needs review", c.review),
                        PageResult::Passes => ("✔ No failures", c.pass),
                        PageResult::Error => ("✖ Couldn't scan", c.fail),
                    };
                    row.col(|ui| {
                        ui.add(egui::Label::new(RichText::new((i + 1).to_string()).color(c.muted)).selectable(false));
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(RichText::new(tag).color(color).strong()).selectable(false));
                    });
                    let num = |ui: &mut egui::Ui, n: Option<usize>| {
                        let text = n.map(|n| n.to_string()).unwrap_or_default();
                        ui.add(egui::Label::new(text).selectable(false));
                    };
                    let report = result.as_ref().and_then(|r| r.as_ref().ok());
                    row.col(|ui| num(ui, report.map(|r| r.outcomes(Outcome::Fail))));
                    row.col(|ui| num(ui, report.map(|r| r.total(Severity::Error))));
                    row.col(|ui| num(ui, report.map(|r| r.total(Severity::Warning))));
                    row.col(|ui| {
                        match result {
                            Some(Ok(r)) => {
                                let title = r.page_title.as_deref().unwrap_or("(no title)");
                                ui.add(egui::Label::new(title).truncate().selectable(false)).on_hover_text(title);
                            }
                            Some(Err(e)) => {
                                ui.add(egui::Label::new(RichText::new(e).color(c.fail)).truncate().selectable(false)).on_hover_text(e);
                            }
                            None => {}
                        };
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(RichText::new(url).color(c.muted)).truncate().selectable(false)).on_hover_text(url);
                    });
                    if row.response().clicked() && report.is_some() {
                        clicked = Some(i);
                    }
                });
            });
        clicked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_a_list_of_local_pages() {
        let dir = std::env::temp_dir().join(format!("adachecker-batch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.html");
        let bad = dir.join("bad.html");
        std::fs::write(&good, r##"<html lang="en"><head><title>Good</title></head><body><a href="#main">Skip</a><main id="main"><h1>Hi</h1></main></body></html>"##).unwrap();
        std::fs::write(&bad, "<html><body><img src=a.png></body></html>").unwrap();
        // The worker pool takes any address the scanner accepts, including local files.
        let urls = vec![good.display().to_string(), bad.display().to_string(), dir.join("missing.html").display().to_string()];
        let run = links::start_pool(urls.clone(), WORKERS, Arc::new(|| {}), |_c, u| scanner::scan(u, Version::V2_1, Level::AA));
        let mut b = BatchScan { rows: urls.iter().map(|u| (u.clone(), None)).collect(), run: Some(run), ..Default::default() };
        let deadline = Instant::now() + Duration::from_secs(20);
        while b.is_running() && Instant::now() < deadline {
            b.poll();
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::remove_dir_all(&dir).ok();
        let results: Vec<PageResult> = b.rows.iter().map(|(_, r)| page_result(r)).collect();
        assert!(matches!(results[0], PageResult::Passes | PageResult::Review), "good page has no failures: {:?}", results[0]);
        assert_eq!(results[1], PageResult::Fails);
        assert_eq!(results[2], PageResult::Error, "a page that can't be read is reported, not dropped");
        assert_eq!(b.report(0).and_then(|r| r.page_title.clone()).as_deref(), Some("Good"));
        assert!(b.report(2).is_none());
        let csv = b.to_csv();
        assert_eq!(csv.lines().count(), 4);
        assert!(csv.lines().nth(2).unwrap().contains(",fails,"), "{csv}");
        assert!(csv.lines().nth(3).unwrap().contains(",error,"), "{csv}");
    }
}
