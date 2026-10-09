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
pub const HISTORY_LEN: usize = 180;

/// Integrates a wrapping energy counter into power figures.
#[derive(Debug)]
pub struct EnergyIntegrator {
    max_range_uj: u64,
    window_s: f64,
    last_raw: Option<u64>,
    total_j: f64,
    /// (time s, cumulative J)
    samples: VecDeque<(f64, f64)>,
}

impl EnergyIntegrator {
    pub fn new(max_range_uj: u64, window_s: f64) -> Self {
        Self { max_range_uj, window_s, last_raw: None, total_j: 0.0, samples: VecDeque::new() }
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
        // Keep the newest sample that is at least `window_s` old as the anchor.
        while self.samples.len() > 2 && self.samples[1].0 <= t - self.window_s {
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

    /// Average power over (up to) the window.
    pub fn average_w(&self) -> Option<f64> {
        let (t0, e0) = *self.samples.front()?;
        let (t1, e1) = *self.samples.back()?;
        (t1 > t0).then(|| (e1 - e0) / (t1 - t0))
    }
}

/// Windowed average of an instantaneous reading (e.g. battery power_now).
#[derive(Debug, Default)]
pub struct WindowAverage {
    window_s: f64,
    samples: VecDeque<(f64, f64)>,
}

impl WindowAverage {
    pub fn new(window_s: f64) -> Self {
        Self { window_s, samples: VecDeque::new() }
    }

    pub fn push(&mut self, t: f64, v: f64) {
        self.samples.push_back((t, v));
        while self.samples.front().is_some_and(|&(t0, _)| t0 < t - self.window_s) {
            self.samples.pop_front();
        }
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }

    pub fn average(&self) -> Option<f64> {
        if self.samples.is_empty() {
            return None;
        }
        Some(self.samples.iter().map(|s| s.1).sum::<f64>() / self.samples.len() as f64)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
        Some(Counter { path, integ: EnergyIntegrator::new(max, AVERAGE_WINDOW_S) })
    }
}

pub struct PowerMonitor {
    package: Option<Counter>,
    psys: Option<Counter>,
    battery_dir: Option<PathBuf>,
    battery_avg: WindowAverage,
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
            battery_avg: WindowAverage::new(AVERAGE_WINDOW_S),
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

    /// Take one sample at time `t` (seconds, monotonic).
    pub fn sample(&mut self, fs: &dyn Sysfs, t: f64) -> PowerReading {
        let mut r = PowerReading { window_s: AVERAGE_WINDOW_S, ..Default::default() };
        for (counter, now, avg) in [
            (&mut self.package, &mut r.package_w, &mut r.package_avg_w),
            (&mut self.psys, &mut r.psys_w, &mut r.psys_avg_w),
        ] {
            if let Some(c) = counter {
                if let Ok(raw) = fs.read_u64(&c.path) {
                    c.integ.push(t, raw);
                }
                *now = c.integ.instant_w();
                *avg = c.integ.average_w();
            }
        }

        if !self.mains.is_empty() {
            r.on_ac = Some(self.mains.iter().any(|p| fs.read_trimmed(p).ok().as_deref() == Some("1")));
        }
        if let Some(dir) = &self.battery_dir {
            r.battery_percent = fs.read_u64(&dir.join("capacity")).ok().map(|v| v as f64);
            let status = fs.read_trimmed(&dir.join("status")).ok();
            if status.as_deref() == Some("Discharging") {
                if let Some(w) = Self::battery_power_w(fs, dir) {
                    self.battery_avg.push(t, w);
                    r.battery_w = Some(w);
                    r.battery_avg_w = self.battery_avg.average();
                }
            } else {
                self.battery_avg.clear();
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
        assert!((e.average_w().unwrap() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn window_slides() {
        let mut e = EnergyIntegrator::new(u64::MAX / 2, 30.0);
        let mut raw = 0u64;
        for t in 0..=100 {
            e.push(t as f64, raw);
            raw += if t < 50 { 20_000_000 } else { 4_000_000 };
        }
        // Last 30 s were all at 4 W.
        assert!((e.average_w().unwrap() - 4.0).abs() < 1e-9);
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
        assert_eq!(m.history().len(), 2);
    }
}
