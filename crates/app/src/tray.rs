//! System tray icon (StatusNotifierItem): estimated battery time left
//! ("2.5h", "30m") on a badge coloured by the active profile, power details
//! in the tooltip, profile switching in the menu. Click opens/closes the window.

use crate::cli::fmt_w;
use crate::client::Client;
use crate::instance;
use ksni::blocking::TrayMethods;
use ksni::menu::{MenuItem, RadioGroup, RadioItem, StandardItem};
use std::process::ExitCode;
use std::time::Duration;
use thinkpower_core::config::ProfileId;
use thinkpower_core::monitor::format_duration;
use thinkpower_core::status::Status;
use zbus::proxy;

const POLL_EVERY: Duration = Duration::from_secs(2);

#[proxy(
    interface = "org.freedesktop.UPower.PowerProfiles",
    default_service = "org.freedesktop.UPower.PowerProfiles",
    default_path = "/org/freedesktop/UPower/PowerProfiles",
    gen_async = false,
    gen_blocking = true,
    blocking_name = "PowerProfilesProxy"
)]
trait PowerProfiles {
    #[zbus(property)]
    fn active_profile(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn set_active_profile(&self, value: &str) -> zbus::Result<()>;
}

/// Switch the desktop profile the same way KDE does (no password needed).
fn switch_profile(id: ProfileId) {
    std::thread::spawn(move || {
        let res = zbus::blocking::Connection::system()
            .and_then(|c| PowerProfilesProxy::new(&c)?.set_active_profile(id.ppd_name()));
        if let Err(e) = res {
            eprintln!("cannot switch profile: {e}");
        }
    });
}

#[derive(Default)]
struct Tray {
    status: Option<Status>,
    error: Option<String>,
}

fn profile_rgb(p: Option<ProfileId>) -> [u8; 3] {
    match p {
        Some(ProfileId::PowerSave) => [46, 157, 87],
        Some(ProfileId::Balanced) => [52, 120, 210],
        Some(ProfileId::Performance) => [222, 98, 40],
        None => [120, 120, 120],
    }
}

/// Compact variable-width bitmap font, 7 rows tall ('#' = lit). Narrow
/// enough that "2.5h" fits a 22 px tray icon.
fn glyph(c: char) -> [&'static str; 7] {
    match c {
        '0' => [".##.", "#..#", "#..#", "#..#", "#..#", "#..#", ".##."],
        '1' => ["..#.", ".##.", "..#.", "..#.", "..#.", "..#.", ".###"],
        '2' => [".##.", "#..#", "...#", "..#.", ".#..", "#...", "####"],
        '3' => ["###.", "...#", "...#", ".##.", "...#", "...#", "###."],
        '4' => ["..#.", ".##.", "#.#.", "#.#.", "####", "..#.", "..#."],
        '5' => ["####", "#...", "###.", "...#", "...#", "#..#", ".##."],
        '6' => [".##.", "#...", "#...", "###.", "#..#", "#..#", ".##."],
        '7' => ["####", "...#", "..#.", "..#.", ".#..", ".#..", ".#.."],
        '8' => [".##.", "#..#", "#..#", ".##.", "#..#", "#..#", ".##."],
        '9' => [".##.", "#..#", "#..#", ".###", "...#", "...#", ".##."],
        '.' => [".", ".", ".", ".", ".", ".", "#"],
        'h' => ["#...", "#...", "#.#.", "##.#", "#..#", "#..#", "#..#"],
        'm' => [".....", ".....", "##.#.", "#.#.#", "#.#.#", "#.#.#", "#.#.#"],
        'A' => [".##.", "#..#", "#..#", "####", "#..#", "#..#", "#..#"],
        'C' => [".###", "#...", "#...", "#...", "#...", "#...", ".###"],
        '!' => ["#", "#", "#", "#", "#", ".", "#"],
        _ => ["...", "...", "...", "###", "...", "...", "..."], // '-'
    }
}

/// Short duration for the badge: "45m", "2.5h", "3h", "12h".
fn short_duration(secs: f64) -> String {
    let mins = (secs / 60.0).round();
    if mins < 60.0 {
        return format!("{}m", mins.max(1.0) as u64);
    }
    let hours = secs / 3600.0;
    if hours < 9.95 {
        let h = (hours * 10.0).round() / 10.0;
        if h.fract() == 0.0 { format!("{h:.0}h") } else { format!("{h:.1}h") }
    } else {
        format!("{}h", (hours.round() as u64).min(99))
    }
}

/// Render a rounded badge with white text as an ARGB32 (network order) icon.
fn badge(size: i32, rgb: [u8; 3], text: &str) -> ksni::Icon {
    let n = size as f64;
    // Lay glyphs out left to right with a 1-unit gap: (x offset, glyph).
    let mut layout = Vec::new();
    let mut units_w = 0usize;
    for c in text.chars() {
        if !layout.is_empty() {
            units_w += 1;
        }
        let g = glyph(c);
        layout.push((units_w, g));
        units_w += g[0].len();
    }
    let units_w = units_w.max(1) as f64;
    let unit = (n * 0.86 / units_w).min(n * 0.62 / 7.0);
    let (tw, th) = (units_w * unit, 7.0 * unit);
    let (ox, oy) = ((n - tw) / 2.0, (n - th) / 2.0);
    let margin = n * 0.02;
    let radius = n * 0.22;

    let in_badge = |x: f64, y: f64| {
        let (lo, hi) = (margin, n - margin);
        if x < lo || x > hi || y < lo || y > hi {
            return false;
        }
        let cx = x.clamp(lo + radius, hi - radius);
        let cy = y.clamp(lo + radius, hi - radius);
        (x - cx).powi(2) + (y - cy).powi(2) <= radius * radius
    };
    let in_text = |x: f64, y: f64| {
        let (u, v) = ((x - ox) / unit, (y - oy) / unit);
        if u < 0.0 || v < 0.0 || v >= 7.0 {
            return false;
        }
        let (col, row) = (u as usize, v as usize);
        layout.iter().any(|(x0, g)| col >= *x0 && g[row].as_bytes().get(col - x0) == Some(&b'#'))
    };

    const SS: usize = 4;
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (mut cover, mut white) = (0usize, 0usize);
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = px as f64 + (sx as f64 + 0.5) / SS as f64;
                    let y = py as f64 + (sy as f64 + 0.5) / SS as f64;
                    if in_badge(x, y) {
                        cover += 1;
                        if in_text(x, y) {
                            white += 1;
                        }
                    }
                }
            }
            let a = (cover * 255 / (SS * SS)) as u8;
            let mix = |c: u8| if cover == 0 { 0 } else { ((c as usize * (cover - white) + 255 * white) / cover) as u8 };
            data.extend_from_slice(&[a, mix(rgb[0]), mix(rgb[1]), mix(rgb[2])]);
        }
    }
    ksni::Icon { width: size, height: size, data }
}

/// Whole-laptop draw over the last minute; falls back to the 30 s average
/// when talking to an older daemon.
fn system_avg_w(p: &thinkpower_core::monitor::PowerReading) -> Option<f64> {
    p.system_minute_avg_w().or(p.battery_avg_w.filter(|_| p.battery_w.is_some())).or(p.psys_avg_w)
}

impl Tray {
    fn active(&self) -> Option<ProfileId> {
        self.status.as_ref().and_then(|s| s.active)
    }

    fn badge_text(&self) -> String {
        if self.error.is_some() {
            return "!".into();
        }
        let Some(p) = self.status.as_ref().map(|s| &s.power) else { return "-".into() };
        if let Some(left) = p.battery_time_left_s {
            short_duration(left)
        } else if p.battery_w.is_none() && p.on_ac == Some(true) {
            // Time to full stays in the tooltip; "+40m" is unreadable at 22 px.
            "AC".into()
        } else {
            // Discharging, but less than a sample of history yet.
            "-".into()
        }
    }

    fn battery_line(&self) -> Option<String> {
        let p = &self.status.as_ref()?.power;
        let pct = p.battery_percent.map(|v| format!("{v:.0} %")).unwrap_or_default();
        Some(if let Some(left) = p.battery_time_left_s {
            format!("Battery {pct}: about {} left", format_duration(left))
        } else if p.battery_w.is_some() {
            format!("Battery {pct}: estimating…")
        } else if let Some(full) = p.battery_time_to_full_s {
            format!("Battery {pct}: charging, full in about {}", format_duration(full))
        } else if p.battery_charge_w.is_some() {
            format!("Battery {pct}: charging")
        } else if p.battery_percent.is_some() {
            format!("Battery {pct}: on AC")
        } else {
            return None;
        })
    }

    fn draw_line(&self) -> Option<String> {
        let p = &self.status.as_ref()?.power;
        let w = system_avg_w(p)?;
        let what = if p.battery_w.is_some() { "Laptop draws" } else { "Platform draws" };
        Some(format!("{what} {w:.1} W (1 min average)"))
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "thinkpower".into()
    }

    fn title(&self) -> String {
        "ThinkPower".into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::Hardware
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let rgb = if self.error.is_some() { [190, 50, 40] } else { profile_rgb(self.active()) };
        let text = self.badge_text();
        [22, 32, 44, 64].into_iter().map(|s| badge(s, rgb, &text)).collect()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let title = match self.active() {
            Some(p) => format!("ThinkPower · {p}"),
            None => "ThinkPower".into(),
        };
        let mut lines = Vec::new();
        if let Some(e) = &self.error {
            lines.push(e.clone());
        }
        lines.extend(self.battery_line());
        lines.extend(self.draw_line());
        if let Some(s) = &self.status {
            let p = &s.power;
            lines.push(format!("CPU package {} · platform {} (1 min)", fmt_w(p.package_minute_avg_w), fmt_w(p.psys_minute_avg_w)));
            let mut limits = Vec::new();
            if let Some(e) = s.effective {
                limits.push(format!("PL1 {:.0} W · PL2 {}", e.pl1_w, e.pl2_w.map(|v| format!("{v:.0} W")).unwrap_or("—".into())));
            }
            if let Some(t) = p.cpu_temp_c {
                limits.push(format!("CPU {t:.0} °C"));
            }
            if !limits.is_empty() {
                lines.push(limits.join(" · "));
            }
        }
        ksni::ToolTip { title, description: lines.join("<br/>"), ..Default::default() }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        std::thread::spawn(instance::toggle_or_launch);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = vec![
            StandardItem {
                label: "Open ThinkPower".into(),
                icon_name: "preferences-system-power".into(),
                activate: Box::new(|_| {
                    std::thread::spawn(instance::open);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
        ];
        for line in [self.battery_line(), self.draw_line()].into_iter().flatten() {
            items.push(StandardItem { label: line, enabled: false, ..Default::default() }.into());
        }
        items.push(MenuItem::Separator);
        let selected = self.active().and_then(|a| ProfileId::ALL.iter().position(|p| *p == a)).unwrap_or(usize::MAX);
        items.push(
            RadioGroup {
                selected,
                select: Box::new(|this: &mut Self, i| {
                    if let Some(&id) = ProfileId::ALL.get(i) {
                        if let Some(s) = this.status.as_mut() {
                            s.active = Some(id);
                        }
                        switch_profile(id);
                    }
                }),
                options: ProfileId::ALL
                    .iter()
                    .map(|p| RadioItem { label: p.label().into(), enabled: self.status.is_some(), ..Default::default() })
                    .collect(),
            }
            .into(),
        );
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Hide tray icon".into(),
                icon_name: "window-close".into(),
                activate: Box::new(|_| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

/// Run the tray until killed. Only one tray per session.
pub fn run() -> ExitCode {
    let lock_dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let lock = std::fs::File::create(lock_dir.join("thinkpower-tray.lock")).and_then(|f| f.try_lock().map(|()| f).map_err(std::io::Error::from));
    let _lock = match lock {
        Ok(f) => f,
        Err(_) => {
            eprintln!("thinkpower tray is already running");
            return ExitCode::SUCCESS;
        }
    };

    let handle = match Tray::default().spawn() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("cannot create tray icon: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut client: Option<Client> = None;
    while !handle.is_closed() {
        if client.is_none() {
            client = Client::connect().ok();
        }
        let res = match &client {
            Some(c) => c.status(),
            None => Err("ThinkPower daemon is not running".into()),
        };
        if res.is_err() {
            client = None;
        }
        handle.update(|t| match res {
            Ok(s) => {
                t.status = Some(s);
                t.error = None;
            }
            Err(e) => {
                t.status = None;
                t.error = Some(e);
            }
        });
        std::thread::sleep(POLL_EVERY);
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_durations() {
        assert_eq!(short_duration(30.0 * 60.0), "30m");
        assert_eq!(short_duration(10.0), "1m");
        assert_eq!(short_duration(59.7 * 60.0), "1h");
        assert_eq!(short_duration(2.5 * 3600.0), "2.5h");
        assert_eq!(short_duration(3.0 * 3600.0 + 60.0), "3h");
        assert_eq!(short_duration(9.97 * 3600.0), "10h");
        assert_eq!(short_duration(12.4 * 3600.0), "12h");
    }

    #[test]
    fn every_badge_glyph_is_rectangular() {
        for c in "0123456789.hmAC!-".chars() {
            let g = glyph(c);
            assert!(g.iter().all(|r| r.len() == g[0].len()), "{c}");
        }
    }

    #[test]
    fn badge_has_opaque_center_and_white_digits() {
        let icon = badge(22, [52, 120, 210], "12");
        assert_eq!(icon.data.len(), 22 * 22 * 4);
        let px = |x: usize, y: usize| &icon.data[(y * 22 + x) * 4..(y * 22 + x) * 4 + 4];
        // Corners are transparent, edges of the badge are the profile colour.
        assert_eq!(px(0, 0)[0], 0);
        assert_eq!(px(11, 2), &[255, 52, 120, 210]);
        // Some pixel in the text area is white.
        assert!((5..17).any(|y| (2..20).any(|x| px(x, y) == [255, 255, 255, 255])));
    }
}
