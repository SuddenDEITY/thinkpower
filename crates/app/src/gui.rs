//! egui front-end. Runs as the normal user; all privileged work goes through
//! the daemon (polkit-authorized) or pkexec for restore.

use crate::cli::{fmt_s, fmt_w};
use crate::client::Client;
use crate::instance::{self, Claim, Remote};
use eframe::egui::{self, Color32, RichText};
use egui_plot::{HLine, Legend, Line, Plot, PlotPoints};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thinkpower_core::caps::Capabilities;
use thinkpower_core::config::{Config, ProfileId};
use thinkpower_core::monitor::format_duration;
use thinkpower_core::rapl::Outcome;
use thinkpower_core::status::Status;
use thinkpower_core::validate::{self, Field, Issue, Severity};

const TEMPLATE: &str = include_str!("../../../configs/x1-nano-gen3.toml");
const POLL_EVERY: Duration = Duration::from_secs(1);

#[derive(Default)]
struct Shared {
    status: Option<Status>,
    conn_error: Option<String>,
    /// Result of the last Apply / Restore, picked up by the UI thread.
    action_result: Option<Result<String, String>>,
    busy: bool,
}

pub struct App {
    shared: Arc<Mutex<Shared>>,
    /// Config as last received from the daemon.
    base: Option<Config>,
    /// What the user is editing.
    draft: Option<Config>,
    message: Option<(bool, String)>,
    confirm_restore: bool,
    /// Shared with the session-bus object the tray talks to.
    remote: Remote,
}

fn spawn_poller(shared: Arc<Mutex<Shared>>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let mut client: Option<Client> = None;
        loop {
            if client.is_none() {
                match Client::connect() {
                    Ok(c) => client = Some(c),
                    Err(e) => shared.lock().unwrap().conn_error = Some(e),
                }
            }
            if let Some(c) = &client {
                match c.status() {
                    Ok(s) => {
                        let mut sh = shared.lock().unwrap();
                        sh.status = Some(s);
                        sh.conn_error = None;
                    }
                    Err(e) => {
                        shared.lock().unwrap().conn_error = Some(e);
                        client = None;
                    }
                }
            }
            ctx.request_repaint();
            std::thread::sleep(POLL_EVERY);
        }
    });
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, remote: Remote) -> Self {
        *remote.ctx.lock().unwrap() = Some(cc.egui_ctx.clone());
        let shared = Arc::new(Mutex::new(Shared::default()));
        spawn_poller(shared.clone(), cc.egui_ctx.clone());
        App { shared, base: None, draft: None, message: None, confirm_restore: false, remote }
    }

    fn dirty(&self) -> bool {
        self.draft.is_some() && self.draft != self.base
    }

    /// Follow the daemon's config unless the user has unsaved edits.
    fn sync_config(&mut self, status: Option<&Status>) {
        let incoming = status.and_then(|s| s.config.clone());
        if incoming.is_none() && self.draft.is_none() {
            // Nothing installed yet: start from the reference profile.
            self.draft = Config::from_toml(TEMPLATE).ok();
            return;
        }
        if incoming.is_some() && incoming != self.base {
            if !self.dirty() {
                self.draft = incoming.clone();
            }
            self.base = incoming;
        }
    }

    fn apply(&mut self, ctx: &egui::Context) {
        let Some(cfg) = self.draft.clone() else { return };
        let shared = self.shared.clone();
        shared.lock().unwrap().busy = true;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let res = Client::connect().and_then(|c| c.set_config(&cfg));
            let mut sh = shared.lock().unwrap();
            sh.busy = false;
            sh.action_result = Some(match res {
                Ok(s) => {
                    let ok = s.last_apply.as_ref().is_none_or(|r| r.success);
                    sh.status = Some(s);
                    if ok { Ok("Saved and applied.".into()) } else { Err("Saved, but hardware did not accept every value — see the active profile.".into()) }
                }
                Err(e) => Err(e),
            });
            ctx.request_repaint();
        });
    }

    fn restore(&mut self, ctx: &egui::Context) {
        let shared = self.shared.clone();
        shared.lock().unwrap().busy = true;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let res = std::process::Command::new("pkexec").args([thinkpower_core::RESTORE_HELPER, "--yes"]).output();
            let mut sh = shared.lock().unwrap();
            sh.busy = false;
            sh.action_result = Some(match res {
                Ok(o) if o.status.success() => Ok("ThinkPower removed; Fedora defaults restored. You can close this window.".into()),
                Ok(o) => Err(format!("Restore failed: {}", String::from_utf8_lossy(&o.stderr).trim())),
                Err(e) => Err(format!("pkexec: {e}")),
            });
            ctx.request_repaint();
        });
    }

    fn import(&mut self, caps: &Capabilities) {
        let Some(path) = rfd::FileDialog::new().add_filter("ThinkPower config", &["toml"]).pick_file() else { return };
        match Config::load(&path) {
            Ok(cfg) => {
                let issues = validate::validate(&cfg, Some(caps));
                let note = cfg
                    .machine
                    .as_ref()
                    .and_then(|m| m.model.clone())
                    .filter(|m| *m != caps.machine_model)
                    .map(|m| format!(" Made for {m}."))
                    .unwrap_or_default();
                self.draft = Some(cfg);
                self.message = Some(if validate::has_errors(&issues) {
                    (false, format!("Imported {} — some values don't fit this hardware; review below, then Apply.{note}", path.display()))
                } else {
                    (true, format!("Imported {} — review, then Apply.{note}", path.display()))
                });
            }
            Err(e) => self.message = Some((false, e.to_string())),
        }
    }

    fn export(&mut self, caps: &Capabilities) {
        let Some(mut cfg) = self.draft.clone() else { return };
        let Some(path) = rfd::FileDialog::new().add_filter("ThinkPower config", &["toml"]).set_file_name("thinkpower.toml").save_file()
        else {
            return;
        };
        let m = cfg.machine.get_or_insert_with(Default::default);
        m.model.get_or_insert(caps.machine_model.clone());
        m.cpu.get_or_insert(caps.cpu_model.clone());
        self.message = Some(match std::fs::write(&path, cfg.to_toml()) {
            Ok(()) => (true, format!("Exported to {}", path.display())),
            Err(e) => (false, format!("{}: {e}", path.display())),
        });
    }
}

fn green(dark: bool) -> Color32 {
    if dark { Color32::from_rgb(110, 200, 120) } else { Color32::from_rgb(30, 130, 50) }
}
fn amber(dark: bool) -> Color32 {
    if dark { Color32::from_rgb(230, 180, 80) } else { Color32::from_rgb(170, 110, 0) }
}
fn red(dark: bool) -> Color32 {
    if dark { Color32::from_rgb(240, 110, 100) } else { Color32::from_rgb(190, 40, 30) }
}

fn outcome_color(o: &Outcome, dark: bool) -> Color32 {
    match o {
        Outcome::Exact => green(dark),
        Outcome::Rounded | Outcome::Unsupported => amber(dark),
        Outcome::Mismatch | Outcome::Rejected(_) => red(dark),
    }
}

/// Small status dot (the default egui fonts have no U+25CF).
fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.5, color);
}

fn stat_tile(ui: &mut egui::Ui, title: &str, big: String, small: String) {
    egui::Frame::group(ui.style()).inner_margin(10.0).show(ui, |ui| {
        ui.set_min_width(170.0);
        ui.label(RichText::new(title).small().weak());
        ui.label(RichText::new(big).size(26.0).strong());
        ui.label(RichText::new(small).small());
    });
}

impl App {
    fn power_section(&self, ui: &mut egui::Ui, s: &Status) {
        let p = &s.power;
        let avg = |v: Option<f64>| format!("{:.0} s average: {}", p.window_s, fmt_w(v));
        ui.horizontal_wrapped(|ui| {
            stat_tile(ui, "CPU PACKAGE", fmt_w(p.package_w), avg(p.package_avg_w));
            if p.psys_w.is_some() {
                stat_tile(ui, "PLATFORM (PSYS)", fmt_w(p.psys_w), avg(p.psys_avg_w));
            }
            let pct = p.battery_percent.map(|v| format!("{v:.0} %")).unwrap_or_default();
            match (p.battery_w, p.on_ac) {
                (Some(_), _) => {
                    stat_tile(ui, "BATTERY DRAW", fmt_w(p.battery_w), avg(p.battery_avg_w));
                    stat_tile(
                        ui,
                        "BATTERY LEFT",
                        p.battery_time_left_s.map(format_duration).unwrap_or_else(|| "estimating…".into()),
                        format!("{pct} at {} (1 min average)", fmt_w(p.battery_minute_avg_w)),
                    );
                }
                (None, Some(true)) => stat_tile(
                    ui,
                    "BATTERY",
                    if pct.is_empty() { "AC".into() } else { pct },
                    match (p.battery_charge_w, p.battery_time_to_full_s) {
                        (Some(w), Some(t)) => format!("charging {w:.1} W, full in {}", format_duration(t)),
                        (Some(w), None) => format!("charging {w:.1} W"),
                        _ => format!("on AC, {}", p.battery_status.as_deref().unwrap_or("").to_lowercase()),
                    },
                ),
                _ => {}
            }
            stat_tile(ui, "CPU TEMPERATURE", p.cpu_temp_c.map(|t| format!("{t:.0} °C")).unwrap_or_else(|| "—".into()), String::new());
        });

        let now = s.history.last().map(|h| h.t).unwrap_or(0.0);
        let series = |f: fn(&thinkpower_core::monitor::HistoryPoint) -> Option<f32>| -> PlotPoints<'static> {
            s.history.iter().filter_map(|h| f(h).map(|v| [h.t - now, v as f64])).collect::<Vec<_>>().into()
        };
        let pl1 = s.effective.map(|e| e.pl1_w);
        Plot::new("power")
            .height(150.0)
            .legend(Legend::default())
            .allow_drag(false)
            .allow_zoom(false)
            .allow_scroll(false)
            .include_y(0.0)
            .include_x(-(thinkpower_core::monitor::HISTORY_LEN as f64))
            .include_x(0.0)
            .x_axis_label("seconds")
            .y_axis_label("W")
            .show(ui, |plot| {
                plot.line(Line::new("Package", series(|h| h.package_w)));
                if s.history.iter().any(|h| h.psys_w.is_some()) {
                    plot.line(Line::new("Platform", series(|h| h.psys_w)));
                }
                if let Some(v) = pl1 {
                    plot.hline(HLine::new("PL1 limit", v).style(egui_plot::LineStyle::dashed_loose()));
                }
            });
    }

    fn profile_card(&mut self, ui: &mut egui::Ui, id: ProfileId, s: Option<&Status>, issues: &[Issue]) {
        let dark = ui.visuals().dark_mode;
        let active = s.and_then(|s| s.active) == Some(id);
        let caps = s.map(|s| &s.capabilities);
        let pl1_max = caps.and_then(|c| c.rapl.as_ref()).and_then(|z| z.pl1_max_w);
        let base = self.base.as_ref().map(|b| b.profiles.get(id).clone());
        let Some(draft) = self.draft.as_mut() else { return };

        let mut frame = egui::Frame::group(ui.style()).inner_margin(12.0);
        if active {
            frame = frame.stroke(egui::Stroke::new(2.0, ui.visuals().selection.bg_fill));
        }
        frame.show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.heading(id.label());
                if active {
                    let c = ui.visuals().selection.bg_fill;
                    ui.label(RichText::new("active").color(c).strong());
                    dot(ui, c);
                }
            });
            let p = draft.profiles.get_mut(id);
            let changed = base.as_ref().map(|b| b != p).unwrap_or(false);
            egui::Grid::new(("grid", id)).num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("PL1").on_hover_text("Sustained package power limit");
                ui.add(egui::DragValue::new(&mut p.pl1_w).speed(0.25).range(0.5..=pl1_max.unwrap_or(250.0)).fixed_decimals(1).suffix(" W"));
                ui.end_row();
                ui.label("PL2").on_hover_text("Short boost power limit");
                ui.add(egui::DragValue::new(&mut p.pl2_w).speed(0.25).range(0.5..=250.0).fixed_decimals(1).suffix(" W"));
                ui.end_row();
                ui.label("Tau").on_hover_text("Time window PL1 is averaged over");
                ui.add(egui::DragValue::new(&mut p.tau_s).speed(0.25).range(0.01..=448.0).fixed_decimals(1).suffix(" s"));
                ui.end_row();
            });
            if changed {
                ui.label(RichText::new("modified — not applied yet").small().color(amber(dark)));
            }

            if let Some(r) = s.and_then(|s| s.last_apply.as_ref()).filter(|r| active && r.profile == id) {
                ui.separator();
                ui.label(RichText::new("In hardware now").small().weak());
                egui::Grid::new(("eff", id)).num_columns(2).spacing([12.0, 2.0]).show(ui, |ui| {
                    for (name, f, secs) in [("PL1", &r.pl1, false), ("PL2", &r.pl2, false), ("Tau", &r.tau, true)] {
                        ui.label(name);
                        let v = if secs { fmt_s(f.effective) } else { fmt_w(f.effective) };
                        let note = match &f.outcome {
                            Outcome::Exact => String::new(),
                            Outcome::Rounded => "  (rounded)".into(),
                            Outcome::Mismatch => format!("  (requested {})", if secs { fmt_s(Some(f.requested)) } else { fmt_w(Some(f.requested)) }),
                            Outcome::Rejected(e) => format!("  rejected: {e}"),
                            Outcome::Unsupported => "  not supported".into(),
                        };
                        ui.label(RichText::new(format!("{v}{note}")).color(outcome_color(&f.outcome, dark)));
                        ui.end_row();
                    }
                });
            }

            let mine: Vec<&Issue> = issues.iter().filter(|i| i.profile == id).collect();
            if !mine.is_empty() {
                ui.separator();
            }
            for i in mine {
                ui.horizontal_wrapped(|ui| {
                    let c = if i.severity == Severity::Error { red(dark) } else { amber(dark) };
                    ui.label(RichText::new(&i.message).color(c));
                    if let Some(v) = i.suggested {
                        let unit = if i.field == Field::Tau { "s" } else { "W" };
                        if ui.small_button(format!("Use {v} {unit}")).clicked() {
                            validate::apply_suggestion(draft, i);
                        }
                    }
                });
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &root.ctx().clone();
        let (status, conn_error, busy, result) = {
            let mut sh = self.shared.lock().unwrap();
            (sh.status.clone(), sh.conn_error.clone(), sh.busy, sh.action_result.take())
        };
        if let Some(r) = result {
            self.message = Some(match r {
                Ok(m) => (true, m),
                Err(m) => (false, m),
            });
        }
        self.sync_config(status.as_ref());
        self.remote.dirty.store(self.dirty(), std::sync::atomic::Ordering::Relaxed);
        let caps = status.as_ref().map(|s| s.capabilities.clone()).unwrap_or_default();
        let issues = self.draft.as_ref().map(|d| validate::validate(d, status.as_ref().map(|s| &s.capabilities))).unwrap_or_default();
        let dark = root.visuals().dark_mode;

        egui::Panel::top("top").show(root, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("ThinkPower");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    match (&status, &conn_error) {
                        (_, Some(_)) => {
                            let r = ui.label(RichText::new("daemon not reachable").color(red(dark)));
                            dot(ui, red(dark));
                            r
                        }
                        (Some(s), None) => {
                            let active = s.active.map(|p| p.label().to_string()).or(s.ppd_profile.clone()).unwrap_or("unknown".into());
                            let r = ui.label(RichText::new(active).strong());
                            dot(ui, green(dark));
                            r
                        }
                        _ => ui.label("connecting…"),
                    };
                    if !caps.machine_model.is_empty() {
                        ui.label(RichText::new(&caps.machine_model).weak());
                    }
                });
            });
            ui.add_space(4.0);
        });

        egui::Panel::bottom("bottom").show(root, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let can_apply = self.dirty() && !validate::has_errors(&issues) && !busy && conn_error.is_none();
                if ui.add_enabled(can_apply, egui::Button::new(RichText::new("Apply").strong())).on_hover_text("Saves the config and applies the active profile. Asks for your password.").clicked() {
                    self.message = None;
                    self.apply(ctx);
                }
                if ui.add_enabled(self.dirty() && self.base.is_some(), egui::Button::new("Revert")).clicked() {
                    self.draft = self.base.clone();
                    self.message = None;
                }
                ui.separator();
                if ui.button("Import…").clicked() {
                    self.import(&caps);
                }
                if ui.add_enabled(self.draft.is_some(), egui::Button::new("Export…")).clicked() {
                    self.export(&caps);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(!busy, egui::Button::new("Restore defaults…")).clicked() {
                        self.confirm_restore = true;
                    }
                    if busy {
                        ui.spinner();
                    }
                });
            });
            if let Some((ok, m)) = &self.message {
                ui.label(RichText::new(m).color(if *ok { green(dark) } else { red(dark) }));
            }
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(root, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                if let Some(e) = &conn_error {
                    ui.colored_label(red(dark), e);
                    ui.add_space(8.0);
                }
                if let Some(s) = &status {
                    if let Some(p) = s.capabilities.blocking_problem() {
                        ui.colored_label(red(dark), p);
                    }
                    if let Some(e) = &s.config_error {
                        ui.colored_label(amber(dark), e);
                    }
                    for w in &s.warnings {
                        ui.colored_label(amber(dark), w);
                    }
                    self.power_section(ui, s);
                    ui.add_space(10.0);
                }

                ui.columns(3, |cols| {
                    for (col, id) in cols.iter_mut().zip(ProfileId::ALL) {
                        self.profile_card(col, id, status.as_ref(), &issues);
                    }
                });

                ui.add_space(10.0);
                if let Some(s) = &status {
                    if let Some(m) = s.firmware_mmio {
                        ui.label(
                            RichText::new(format!(
                                "Firmware (MMIO) limits: PL1 {}, PL2 {}. The CPU enforces the lower of these and the values above.",
                                fmt_w(Some(m.pl1_w)),
                                fmt_w(m.pl2_w)
                            ))
                            .small()
                            .weak(),
                        );
                    }
                    if let Some(pp) = &s.platform_profile {
                        ui.label(RichText::new(format!("ACPI platform profile: {pp}")).small().weak());
                    }
                }
                ui.label(
                    RichText::new(
                        "Limits are ceilings, not targets: thermal and firmware protections (DPTF, lap detection) may hold power lower. \
                         Switch profiles from KDE as usual — this window doesn't need to stay open.",
                    )
                    .small()
                    .weak(),
                );
            });
        });

        if self.confirm_restore {
            egui::Window::new("Restore defaults?").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.label("This removes the ThinkPower daemon and its config, and returns power profiles to Fedora's stock tuned behaviour.");
                ui.label("Current RAPL limits stay until the next reboot or profile switch.");
                ui.horizontal(|ui| {
                    if ui.button("Restore").clicked() {
                        self.confirm_restore = false;
                        self.restore(ctx);
                    }
                    if ui.button("Cancel").clicked() {
                        self.confirm_restore = false;
                    }
                });
            });
        }
    }
}

pub fn run() -> eframe::Result<()> {
    let remote = Remote::default();
    // Keep the bus connection alive for as long as the window is open.
    let _instance = match instance::claim(remote.clone()) {
        Claim::AlreadyRunning => return Ok(()),
        Claim::Primary(conn) => Some(conn),
        Claim::Unavailable => None,
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ThinkPower")
            .with_app_id("thinkpower")
            .with_inner_size([980.0, 780.0])
            .with_min_inner_size([760.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native("ThinkPower", options, Box::new(move |cc| Ok(Box::new(App::new(cc, remote)))))
}
