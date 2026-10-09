//! Live power / temperature sampling.
//!
//! RAPL `energy_uj` counters are root-only since the Platypus mitigation, so
//! this runs inside the daemon, which publishes 1 s and windowed averages.

use crate::rapl::{self, CONTROL_MSR};
use crate::sysfs::Sysfs;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

pub const AVERAGE_WINDOW_S: f64 = 30.0;
/// Longer window used for the battery-life estimate and the tray.
pub const MINUTE_WINDOW_S: f64 = 60.0;
pub const HISTORY_LEN: usize = 180;

/// Integrates a wrapping energy counter into power figures.
#[derive(Debug)]
pub struct EnergyIntegrator {
    max_range_uj: u64,
    /// How much history to keep, seconds.
    keep_s: f64,
    last_raw: Option<u64>,
    total_j: f64,
    /// (time s, cumulative J)
    samples: VecDeque<(f64, f64)>,
}

impl EnergyIntegrator {
    pub fn new(max_range_uj: u64, keep_s: f64) -> Self {
        Self { max_range_uj, keep_s, last_raw: None, total_j: 0.0, samples: VecDeque::new() }
    }

    pub fn push(&mut self, t: f64, raw_uj: u64) {
        if let Some(last) = self.last_raw {
            let delta = if raw_uj >= last {
                raw_uj - last
            } else {
                // Counter wrapped.
                raw_uj + self.max_range_uj.saturating_sub(last)
            };
            self.total_j += delta as f64 / 1e6;
        }
        self.last_raw = Some(raw_uj);
        self.samples.push_back((t, self.total_j));
        // Keep the newest sample that is at least `keep_s` old as the anchor.
        while self.samples.len() > 2 && self.samples[1].0 <= t - self.keep_s {
            self.samples.pop_front();
        }
    }

    /// Power over the last sample interval.
    pub fn instant_w(&self) -> Option<f64> {
        let n = self.samples.len();
        if n < 2 {
            return None;
        }
        let (t0, e0) = self.samples[n - 2];
        let (t1, e1) = self.samples[n - 1];
        (t1 > t0).then(|| (e1 - e0) / (t1 - t0))
    }

    /// Average power over the last `secs` (or all history, if shorter).
    pub fn average_over_w(&self, secs: f64) -> Option<f64> {
        let (t1, e1) = *self.samples.back()?;
        let cutoff = t1 - secs;
        let (t0, e0) = *self.samples.iter().rev().find(|s| s.0 <= cutoff).or(self.samples.front())?;
        (t1 > t0).then(|| (e1 - e0) / (t1 - t0))
    }
}

/// Windowed average of an instantaneous reading (e.g. battery power_now).
#[derive(Debug, Default)]
pub struct WindowAverage {
    keep_s: f64,
    samples: VecDeque<(f64, f64)>,
}

impl WindowAverage {
    pub fn new(keep_s: f64) -> Self {
        Self { keep_s, samples: VecDeque::new() }
    }

    pub fn push(&mut self, t: f64, v: f64) {
        self.samples.push_back((t, v));
        while self.samples.front().is_some_and(|&(t0, _)| t0 < t - self.keep_s) {
            self.samples.pop_front();
        }
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// Mean of the samples from the last `secs`.
    pub fn average_over(&self, secs: f64) -> Option<f64> {
        let t1 = self.samples.back()?.0;
        let recent: Vec<f64> = self.samples.iter().filter(|s| s.0 >= t1 - secs).map(|s| s.1).collect();
        Some(recent.iter().sum::<f64>() / recent.len() as f64)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PowerReading {
    pub window_s: f64,
    /// CPU package (what PL1/PL2 limit).
    pub package_w: Option<f64>,
    pub package_avg_w: Option<f64>,
    /// Whole SoC platform ("psys"), if exposed.
    pub psys_w: Option<f64>,
    pub psys_avg_w: Option<f64>,
    /// Battery discharge rate; only set while discharging.
    pub battery_w: Option<f64>,
    pub battery_avg_w: Option<f64>,
    pub battery_percent: Option<f64>,
    pub battery_status: Option<String>,
    pub on_ac: Option<bool>,
    pub cpu_temp_c: Option<f64>,
    /// 60 s averages (see [`MINUTE_WINDOW_S`]).
    pub package_minute_avg_w: Option<f64>,
    pub psys_minute_avg_w: Option<f64>,
    pub battery_minute_avg_w: Option<f64>,
    /// Charging rate; only set while charging.
    pub battery_charge_w: Option<f64>,
    pub battery_energy_wh: Option<f64>,
    pub battery_full_wh: Option<f64>,
    /// Estimated runtime at the last minute's average draw.
    pub battery_time_left_s: Option<f64>,
    /// Estimated time to full at the last minute's average charge rate.
    pub battery_time_to_full_s: Option<f64>,
}

impl PowerReading {
    /// Best whole-laptop figure over the last minute: battery draw when
    /// discharging, else platform (psys) power.
    pub fn system_minute_avg_w(&self) -> Option<f64> {
        self.battery_minute_avg_w.filter(|_| self.battery_w.is_some()).or(self.psys_minute_avg_w)
    }
}

/// "1 h 05 min" / "42 min".
pub fn format_duration(secs: f64) -> String {
    let mins = (secs / 60.0).round() as u64;
    if mins >= 60 { format!("{} h {:02} min", mins / 60, mins % 60) } else { format!("{mins} min") }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HistoryPoint {
    /// Seconds since daemon start.
    pub t: f64,
    pub package_w: Option<f32>,
    pub psys_w: Option<f32>,
}

struct Counter {
    path: PathBuf,
    integ: EnergyIntegrator,
}

impl Counter {
    fn open(fs: &dyn Sysfs, zone_dir: &Path) -> Option<Self> {
        let path = zone_dir.join("energy_uj");
        fs.read_u64(&path).ok()?;
        let max = fs.read_u64(&zone_dir.join("max_energy_range_uj")).unwrap_or(u64::MAX / 2);
        Some(Counter { path, integ: EnergyIntegrator::new(max, MINUTE_WINDOW_S) })
    }
}

pub struct PowerMonitor {
    package: Option<Counter>,
    psys: Option<Counter>,
    battery_dir: Option<PathBuf>,
    battery_avg: WindowAverage,
    battery_last_status: Option<String>,
    mains: Vec<PathBuf>,
    temp_input: Option<PathBuf>,
    history: VecDeque<HistoryPoint>,
}

const POWER_SUPPLY: &str = "/sys/class/power_supply";
const HWMON: &str = "/sys/class/hwmon";

fn find_package_temp(fs: &dyn Sysfs) -> Option<PathBuf> {
    let base = Path::new(HWMON);
    let mut hw = fs.list(base).ok()?;
    hw.sort();
    for h in hw {
        let dir = base.join(&h);
        if fs.read_trimmed(&dir.join("name")).ok().as_deref() != Some("coretemp") {
            continue;
        }
        let mut fallback = None;
        for i in 1..64 {
            let input = dir.join(format!("temp{i}_input"));
            if !fs.exists(&input) {
                continue;
            }
            let label = fs.read_trimmed(&dir.join(format!("temp{i}_label"))).unwrap_or_default();
            if label.starts_with("Package") {
                return Some(input);
            }
            fallback.get_or_insert(input);
        }
        if fallback.is_some() {
            return fallback;
        }
    }
    None
}

impl PowerMonitor {
    pub fn discover(fs: &dyn Sysfs) -> Self {
        let base = Path::new(rapl::POWERCAP);
        let package = rapl::find_zone(fs, base, CONTROL_MSR, |n| n.starts_with("package")).and_then(|(_, d, _)| Counter::open(fs, &d));
        let psys = rapl::find_zone(fs, base, CONTROL_MSR, |n| n == "psys").and_then(|(_, d, _)| Counter::open(fs, &d));

        let mut battery_dir = None;
        let mut mains = Vec::new();
        if let Ok(mut supplies) = fs.list(Path::new(POWER_SUPPLY)) {
            supplies.sort();
            for s in supplies {
                let dir = Path::new(POWER_SUPPLY).join(&s);
                match fs.read_trimmed(&dir.join("type")).unwrap_or_default().as_str() {
                    "Battery" if battery_dir.is_none() && fs.read_trimmed(&dir.join("scope")).ok().as_deref() != Some("Device") => {
                        battery_dir = Some(dir)
                    }
                    "Mains" => mains.push(dir.join("online")),
                    _ => {}
                }
            }
        }

        PowerMonitor {
            package,
            psys,
            battery_dir,
            battery_avg: WindowAverage::new(MINUTE_WINDOW_S),
            battery_last_status: None,
            mains,
            temp_input: find_package_temp(fs),
            history: VecDeque::with_capacity(HISTORY_LEN),
        }
    }

    fn battery_power_w(fs: &dyn Sysfs, dir: &Path) -> Option<f64> {
        if let Ok(uw) = fs.read_u64(&dir.join("power_now")) {
            return Some(uw as f64 / 1e6);
        }
        let ua = fs.read_u64(&dir.join("current_now")).ok()?;
        let uv = fs.read_u64(&dir.join("voltage_now")).ok()?;
        Some(ua as f64 * uv as f64 / 1e12)
    }

    /// (now, full) in Wh, from energy_* (µWh) or charge_* (µAh) × voltage.
    fn battery_energy_wh(fs: &dyn Sysfs, dir: &Path) -> (Option<f64>, Option<f64>) {
        let read = |name: &str| fs.read_u64(&dir.join(name)).ok().map(|v| v as f64);
        if let (Some(now), full) = (read("energy_now"), read("energy_full")) {
            return (Some(now / 1e6), full.map(|f| f / 1e6));
        }
        let Some(uv) = read("voltage_min_design").or_else(|| read("voltage_now")) else { return (None, None) };
        let wh = |uah: f64| uah * uv / 1e12;
        (read("charge_now").map(wh), read("charge_full").map(wh))
    }

    /// Take one sample at time `t` (seconds, monotonic).
    pub fn sample(&mut self, fs: &dyn Sysfs, t: f64) -> PowerReading {
        let mut r = PowerReading { window_s: AVERAGE_WINDOW_S, ..Default::default() };
        for (counter, now, avg, minute) in [
            (&mut self.package, &mut r.package_w, &mut r.package_avg_w, &mut r.package_minute_avg_w),
            (&mut self.psys, &mut r.psys_w, &mut r.psys_avg_w, &mut r.psys_minute_avg_w),
        ] {
            if let Some(c) = counter {
                if let Ok(raw) = fs.read_u64(&c.path) {
                    c.integ.push(t, raw);
                }
                *now = c.integ.instant_w();
                *avg = c.integ.average_over_w(AVERAGE_WINDOW_S);
                *minute = c.integ.average_over_w(MINUTE_WINDOW_S);
            }
        }

        if !self.mains.is_empty() {
            r.on_ac = Some(self.mains.iter().any(|p| fs.read_trimmed(p).ok().as_deref() == Some("1")));
        }
        if let Some(dir) = &self.battery_dir {
            r.battery_percent = fs.read_u64(&dir.join("capacity")).ok().map(|v| v as f64);
            let status = fs.read_trimmed(&dir.join("status")).ok();
            if status != self.battery_last_status {
                // Plugging/unplugging changes the meaning of power_now.
                self.battery_avg.clear();
                self.battery_last_status = status.clone();
            }
            let (energy, full) = Self::battery_energy_wh(fs, dir);
            r.battery_energy_wh = energy;
            r.battery_full_wh = full;
            let power = Self::battery_power_w(fs, dir).filter(|&w| w > 0.0);
            match (status.as_deref(), power) {
                (Some("Discharging"), Some(w)) => {
                    self.battery_avg.push(t, w);
                    r.battery_w = Some(w);
                    r.battery_avg_w = self.battery_avg.average_over(AVERAGE_WINDOW_S);
                    r.battery_minute_avg_w = self.battery_avg.average_over(MINUTE_WINDOW_S);
                    r.battery_time_left_s = match (energy, r.battery_minute_avg_w) {
                        (Some(e), Some(avg)) if avg > 0.1 => Some(e / avg * 3600.0),
                        _ => None,
                    };
                }
                (Some("Charging"), Some(w)) => {
                    self.battery_avg.push(t, w);
                    r.battery_charge_w = Some(w);
                    r.battery_time_to_full_s = match (energy, full, self.battery_avg.average_over(MINUTE_WINDOW_S)) {
                        (Some(e), Some(f), Some(avg)) if avg > 0.1 && f > e => Some((f - e) / avg * 3600.0),
                        _ => None,
                    };
                }
                _ => {}
            }
            r.battery_status = status;
        }
        r.cpu_temp_c = self.temp_input.as_ref().and_then(|p| fs.read_u64(p).ok()).map(|m| m as f64 / 1000.0);

        if self.history.len() == HISTORY_LEN {
            self.history.pop_front();
        }
        self.history.push_back(HistoryPoint { t, package_w: r.package_w.map(|v| v as f32), psys_w: r.psys_w.map(|v| v as f32) });
        r
    }

    pub fn history(&self) -> Vec<HistoryPoint> {
        self.history.iter().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rapl::tests::x1_nano;

    #[test]
    fn instant_and_average() {
        let mut e = EnergyIntegrator::new(1_000_000_000, 30.0);
        assert_eq!(e.instant_w(), None);
        // 5 W for 10 s, then 15 W for 10 s.
        let mut raw = 0u64;
        for t in 0..=20 {
            e.push(t as f64, raw);
            raw += if t < 10 { 5_000_000 } else { 15_000_000 };
        }
        assert!((e.instant_w().unwrap() - 15.0).abs() < 1e-9);
        assert!((e.average_over_w(30.0).unwrap() - 10.0).abs() < 1e-9);
        assert!((e.average_over_w(5.0).unwrap() - 15.0).abs() < 1e-9);
    }

    #[test]
    fn window_slides() {
        let mut e = EnergyIntegrator::new(u64::MAX / 2, 60.0);
        let mut raw = 0u64;
        for t in 0..=100 {
            e.push(t as f64, raw);
            raw += if t < 70 { 20_000_000 } else { 4_000_000 };
        }
        // Last 30 s were all at 4 W; the last 60 s were half 20 W, half 4 W.
        assert!((e.average_over_w(30.0).unwrap() - 4.0).abs() < 1e-9);
        assert!((e.average_over_w(60.0).unwrap() - 12.0).abs() < 1e-9);
        // Older history is dropped.
        assert!((e.average_over_w(1000.0).unwrap() - 12.0).abs() < 1e-9);
    }

    #[test]
    fn window_average_over() {
        let mut w = WindowAverage::new(60.0);
        for t in 0..=90 {
            w.push(t as f64, if t <= 60 { 10.0 } else { 4.0 });
        }
        assert!((w.average_over(29.0).unwrap() - 4.0).abs() < 1e-9);
        assert!(w.average_over(60.0).unwrap() > 4.0);
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(42.0 * 60.0), "42 min");
        assert_eq!(format_duration(3900.0), "1 h 05 min");
    }

    #[test]
    fn counter_wrap() {
        let max = 262_143_328_850u64;
        let mut e = EnergyIntegrator::new(max, 30.0);
        e.push(0.0, max - 3_000_000);
        e.push(1.0, 2_000_000);
        assert!((e.instant_w().unwrap() - 5.0).abs() < 1e-9);
    }

    #[test]
    fn monitor_samples_mock() {
        let fs = x1_nano();
        fs.set("/sys/class/power_supply/AC/type", "Mains")
            .set("/sys/class/power_supply/AC/online", "0")
            .set("/sys/class/power_supply/BAT0/type", "Battery")
            .set("/sys/class/power_supply/BAT0/status", "Discharging")
            .set("/sys/class/power_supply/BAT0/capacity", "78")
            .set("/sys/class/power_supply/BAT0/power_now", "7400000")
            .set("/sys/class/power_supply/BAT0/energy_now", "18500000")
            .set("/sys/class/power_supply/BAT0/energy_full", "41050000")
            .set("/sys/class/hwmon/hwmon8/name", "coretemp")
            .set("/sys/class/hwmon/hwmon8/temp1_label", "Package id 0")
            .set("/sys/class/hwmon/hwmon8/temp1_input", "62000")
            .set("/sys/class/hwmon/hwmon8/temp2_label", "Core 0")
            .set("/sys/class/hwmon/hwmon8/temp2_input", "55000");
        let mut m = PowerMonitor::discover(&fs);
        m.sample(&fs, 0.0);
        fs.set("/sys/class/powercap/intel-rapl:0/energy_uj", "8000000"); // +7 J
        fs.set("/sys/class/powercap/intel-rapl:1/energy_uj", "17000000"); // +12 J
        let r = m.sample(&fs, 1.0);
        assert!((r.package_w.unwrap() - 7.0).abs() < 1e-9);
        assert!((r.psys_w.unwrap() - 12.0).abs() < 1e-9);
        assert_eq!(r.battery_w, Some(7.4));
        assert_eq!(r.battery_percent, Some(78.0));
        assert_eq!(r.on_ac, Some(false));
        assert_eq!(r.cpu_temp_c, Some(62.0));
        assert_eq!(r.battery_energy_wh, Some(18.5));
        // 18.5 Wh at 7.4 W = 2.5 h.
        assert!((r.battery_time_left_s.unwrap() - 9000.0).abs() < 1e-6);
        assert_eq!(r.system_minute_avg_w(), Some(7.4));

        fs.set("/sys/class/power_supply/AC/online", "1").set("/sys/class/power_supply/BAT0/status", "Charging");
        fs.set("/sys/class/power_supply/BAT0/power_now", "22550000"); // 22.55 W into a 22.55 Wh gap
        fs.set("/sys/class/powercap/intel-rapl:1/energy_uj", "27000000");
        let r = m.sample(&fs, 2.0);
        assert_eq!(r.battery_w, None);
        assert_eq!(r.battery_charge_w, Some(22.55));
        assert!((r.battery_time_to_full_s.unwrap() - 3600.0).abs() < 1e-6);
        // On AC the system figure falls back to psys.
        assert!(r.system_minute_avg_w().is_some());
        assert_eq!(m.history().len(), 3);
    }
}
