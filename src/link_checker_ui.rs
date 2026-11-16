//! "Link Checker" page: paste links on the left, results table on the right.

use crate::links::{self, Kind, LinkResult, LinkRun, Parsed};
use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct LinkChecker {
    input: String,
    parsed: Parsed,
    /// The links of the current/last run, with results as they arrive.
    rows: Vec<(String, Option<LinkResult>)>,
    run: Option<LinkRun>,
    started: Option<Instant>,
    took: Option<Duration>,
    cancelled: bool,
    only_problems: bool,
    copied: bool,
    autostart: bool,
}

impl Default for LinkChecker {
    fn default() -> Self {
        Self {
            input: String::new(),
            parsed: Parsed::default(),
            rows: Vec::new(),
            run: None,
            started: None,
            took: None,
            cancelled: false,
            only_problems: false,
            copied: false,
            autostart: false,
        }
    }
}

struct Colors {
    ok: Color32,
    redirect: Color32,
    restricted: Color32,
    broken: Color32,
    muted: Color32,
}

/// Status colours with ≥ 4.5:1 contrast on egui's panels; every status also has an
/// icon and a text label so colour is never the only cue.
fn colors(dark: bool) -> Colors {
    if dark {
        Colors {
            ok: Color32::from_rgb(120, 220, 140),
            redirect: Color32::from_rgb(130, 190, 255),
            restricted: Color32::from_rgb(255, 204, 102),
            broken: Color32::from_rgb(255, 138, 128),
            muted: Color32::from_rgb(170, 170, 170),
        }
    } else {
        Colors {
            ok: Color32::from_rgb(0, 110, 40),
            redirect: Color32::from_rgb(0, 82, 170),
            restricted: Color32::from_rgb(138, 76, 0),
            broken: Color32::from_rgb(176, 0, 32),
            muted: Color32::from_rgb(85, 85, 85),
        }
    }
}

fn kind_style(kind: Kind, c: &Colors) -> (&'static str, Color32) {
    match kind {
        Kind::Ok => ("✔", c.ok),
        Kind::Redirected => ("↪", c.redirect),
        Kind::Restricted => ("⚠", c.restricted),
        Kind::Broken => ("✖", c.broken),
        Kind::Error => ("✖", c.broken),
    }
}

impl LinkChecker {
    /// Replaces the pasted text; optionally starts checking on the next frame.
    pub fn load(&mut self, text: String, start: bool) {
        self.parsed = links::parse_links(&text);
        self.input = text;
        self.autostart = start;
    }

    fn is_running(&self) -> bool {
        self.run.is_some()
    }

    fn start(&mut self, ctx: &egui::Context) {
        if self.is_running() || self.parsed.links.is_empty() {
            return;
        }
        self.rows = self.parsed.links.iter().map(|l| (l.clone(), None)).collect();
        let ctx = ctx.clone();
        self.run = Some(links::start(self.parsed.links.clone(), Arc::new(move || ctx.request_repaint())));
        self.started = Some(Instant::now());
        self.took = None;
        self.cancelled = false;
        self.copied = false;
    }

    fn poll(&mut self) {
        let Some(run) = &self.run else { return };
        // The channel disconnects once every worker has exited — after the last link, or
        // after in-flight requests finish when the run was stopped.
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

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.poll();
        if std::mem::take(&mut self.autostart) {
            self.start(ui.ctx());
        }
        egui::Panel::left("links-input")
            .resizable(true)
            .default_size(380.0)
            .min_size(260.0)
            .show(ui, |ui| self.input_pane(ui));
        egui::CentralPanel::default().show(ui, |ui| self.results_pane(ui));
    }

    fn input_pane(&mut self, ui: &mut egui::Ui) {
        let c = colors(ui.visuals().dark_mode);
        ui.add_space(6.0);
        ui.heading("Links to check");
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let n = self.parsed.links.len();
            let label = if n == 1 { "Check 1 link".to_string() } else { format!("Check {n} links") };
            let can_start = !self.is_running() && n > 0;
            if ui
                .add_enabled(can_start, egui::Button::new(RichText::new(label).strong()))
                .on_disabled_hover_text(if n == 0 { "Paste some links first" } else { "A check is already running" })
                .clicked()
            {
                self.start(ui.ctx());
            }
            if self.is_running() {
                if ui.button("Stop").clicked() {
                    if let Some(run) = &self.run {
                        run.cancel();
                    }
                    self.cancelled = true;
                }
                ui.spinner();
            } else if ui.add_enabled(!self.input.is_empty(), egui::Button::new("Clear")).clicked() {
                self.input.clear();
                self.parsed = Parsed::default();
            }
        });
        ui.add_space(4.0);
        let mut summary = match self.parsed.links.len() {
            0 => "No links found yet.".to_string(),
            1 => "1 link found".to_string(),
            n => format!("{n} links found"),
        };
        if self.parsed.duplicates > 0 {
            summary.push_str(&format!(" · {} duplicate(s) ignored", self.parsed.duplicates));
        }
        let summary_label = ui.label(RichText::new(summary).small().color(c.muted));
        ui.add_space(4.0);

        egui::ScrollArea::vertical().id_salt("links-input-scroll").auto_shrink([false, false]).show(ui, |ui| {
            let edit = ui
                .add_sized(
                    ui.available_size(),
                    egui::TextEdit::multiline(&mut self.input)
                        .font(egui::TextStyle::Monospace)
                        .hint_text("Paste links here, one per line.\n\nhttps://example.com\nexample.org/page\n\nLinks inside text, Markdown or HTML are found too.\nLines starting with # are ignored.")
                        .desired_width(f32::INFINITY),
                )
                .labelled_by(summary_label.id);
            if edit.changed() {
                self.parsed = links::parse_links(&self.input);
            }
        });
    }

    fn results_pane(&mut self, ui: &mut egui::Ui) {
        let c = colors(ui.visuals().dark_mode);
        ui.add_space(6.0);
        ui.heading("Results");
        ui.add_space(4.0);

        if self.rows.is_empty() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No results yet.").size(16.0));
                ui.label(RichText::new("Paste links on the left and press Check.").color(c.muted));
            });
            return;
        }

        // Summary counts.
        let count = |k: Kind| self.rows.iter().filter(|(_, r)| r.as_ref().is_some_and(|r| r.kind == k)).count();
        let checked = self.rows.iter().filter(|(_, r)| r.is_some()).count();
        let total = self.rows.len();
        ui.horizontal_wrapped(|ui| {
            for (kind, n) in [
                (Kind::Ok, count(Kind::Ok)),
                (Kind::Redirected, count(Kind::Redirected)),
                (Kind::Restricted, count(Kind::Restricted)),
                (Kind::Broken, count(Kind::Broken)),
                (Kind::Error, count(Kind::Error)),
            ] {
                let (icon, color) = kind_style(kind, &c);
                ui.label(RichText::new(format!("{icon} {} {n}", kind.label())).color(color).strong());
                ui.add_space(8.0);
            }
            if checked < total {
                ui.label(RichText::new(format!("… Pending {}", total - checked)).color(c.muted));
            }
        });

        if self.is_running() || checked < total {
            let text = if self.cancelled { format!("Stopping… {checked} of {total}") } else { format!("Checked {checked} of {total}") };
            ui.add(egui::ProgressBar::new(checked as f32 / total.max(1) as f32).text(text));
        } else if let Some(t) = self.took {
            ui.label(RichText::new(format!("Checked {total} link(s) in {:.1}s", t.as_secs_f64())).small().color(c.muted));
        }

        ui.horizontal(|ui| {
            ui.checkbox(&mut self.only_problems, "Show only problems");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button(if self.copied { "Copied ✔" } else { "Copy as CSV" })
                    .on_hover_text("Copy the results as CSV for a spreadsheet")
                    .clicked()
                {
                    ui.ctx().copy_text(links::to_csv(&self.rows));
                    self.copied = true;
                }
            });
        });
        ui.add_space(4.0);

        let visible: Vec<usize> = (0..self.rows.len())
            .filter(|&i| {
                !self.only_problems
                    || self.rows[i].1.as_ref().is_some_and(|r| !matches!(r.kind, Kind::Ok | Kind::Redirected))
            })
            .collect();
        if visible.is_empty() {
            ui.label(RichText::new("No problems found.").color(c.ok));
            return;
        }

        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(40.0))
            .column(Column::initial(120.0).at_least(90.0))
            .column(Column::initial(170.0).at_least(110.0))
            .column(Column::initial(320.0).at_least(140.0).clip(true))
            .column(Column::remainder().at_least(160.0).clip(true))
            .column(Column::exact(84.0))
            .header(row_h + 2.0, |mut h| {
                for title in ["#", "Result", "HTTP status", "URL", "Details", "Time"] {
                    h.col(|ui| {
                        ui.strong(title);
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, visible.len(), |mut row| {
                    let i = visible[row.index()];
                    let (link, result) = &self.rows[i];
                    row.col(|ui| {
                        ui.label(RichText::new((i + 1).to_string()).color(c.muted));
                    });
                    match result {
                        None => {
                            row.col(|ui| {
                                ui.label(RichText::new(if self.is_running() { "… Checking" } else { "Not checked" }).color(c.muted));
                            });
                            row.col(|ui| {
                                ui.label(RichText::new("—").color(c.muted));
                            });
                        }
                        Some(r) => {
                            let (icon, color) = kind_style(r.kind, &c);
                            row.col(|ui| {
                                ui.label(RichText::new(format!("{icon} {}", r.kind.label())).color(color).strong());
                            });
                            row.col(|ui| {
                                ui.label(RichText::new(r.status_text()).color(color).monospace())
                                    .on_hover_text(format!("Checked with {}", r.method));
                            });
                        }
                    }
                    row.col(|ui| {
                        ui.add(egui::Hyperlink::from_label_and_url(link.as_str(), link.as_str()).open_in_new_tab(true))
                            .on_hover_text(format!("{link}\n\nClick to open in your browser"));
                    });
                    row.col(|ui| {
                        if let Some(r) = result {
                            let details = match r.kind {
                                Kind::Error => r.reason.clone(),
                                _ if !r.hops.is_empty() => format!("{}  →  {}", r.chain_text(), r.final_url),
                                Kind::Restricted => "Refused automated check; may work in a browser".into(),
                                _ => String::new(),
                            };
                            let mut hover = details.clone();
                            for (n, hop) in r.hops.iter().enumerate() {
                                hover.push_str(&format!("\n{}. {} {}", n + 1, hop.status, hop.url));
                            }
                            ui.add(egui::Label::new(RichText::new(&details).color(c.muted)).truncate())
                                .on_hover_text(hover);
                        }
                    });
                    row.col(|ui| {
                        if let Some(r) = result {
                            ui.label(RichText::new(format!("{} ms", r.elapsed.as_millis())).color(c.muted));
                        }
                    });
                });
            });
    }
}
