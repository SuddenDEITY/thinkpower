//! Intel RAPL (powercap) discovery, apply and readback.
//!
//! Zones and constraints are discovered by *name* (`package-0`, `long_term`,
//! `short_term`), never by hardcoded index.

use crate::config::{Limits, ProfileId};
use crate::sysfs::Sysfs;
use crate::units::{s_to_us, uw_to_w, us_to_s, w_to_uw};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const POWERCAP: &str = "/sys/class/powercap";
/// MSR-backed RAPL interface; this is where we write.
pub const CONTROL_MSR: &str = "intel-rapl";
/// MMIO-backed interface, typically driven by firmware/DPTF. Read-only for us;
/// the CPU enforces the lower of the two.
pub const CONTROL_MMIO: &str = "intel-rapl-mmio";

#[derive(Debug, Clone)]
pub struct Constraint {
    pub index: u32,
    pub dir: PathBuf,
}

impl Constraint {
    pub fn power_path(&self) -> PathBuf {
        self.dir.join(format!("constraint_{}_power_limit_uw", self.index))
    }
    pub fn window_path(&self) -> PathBuf {
        self.dir.join(format!("constraint_{}_time_window_us", self.index))
    }
    pub fn max_power_path(&self) -> PathBuf {
        self.dir.join(format!("constraint_{}_max_power_uw", self.index))
    }
}

#[derive(Debug, Clone)]
pub struct RaplZone {
    /// e.g. `intel-rapl:0`
    pub id: String,
    pub dir: PathBuf,
    /// e.g. `package-0`
    pub name: String,
    pub pl1: Constraint,
    pub pl2: Option<Constraint>,
}

/// Limits as read back from hardware.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HwLimits {
    pub pl1_w: f64,
    pub pl2_w: Option<f64>,
    pub tau_s: f64,
}

impl HwLimits {
    /// True if the hardware state moved away from `other` by more than
    /// quantization noise.
    pub fn drifted_from(&self, other: &HwLimits) -> bool {
        let p = |a: f64, b: f64| (a - b).abs() > 0.01;
        p(self.pl1_w, other.pl1_w)
            || match (self.pl2_w, other.pl2_w) {
                (Some(a), Some(b)) => p(a, b),
                (a, b) => a.is_some() != b.is_some(),
            }
            || (self.tau_s - other.tau_s).abs() > 0.01 * other.tau_s.max(0.001)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RaplError {
    #[error("powercap interface not found at {0} (is the intel_rapl driver loaded?)")]
    NoPowercap(String),
    #[error("no package zone found for {0}")]
    NoPackageZone(String),
    #[error("zone {0} has no long_term (PL1) constraint")]
    NoLongTerm(String),
    #[error("{path}: {source}")]
    Io { path: String, source: std::io::Error },
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> RaplError + '_ {
    move |source| RaplError::Io { path: path.display().to_string(), source }
}

/// Find a top-level zone of `control` (e.g. `intel-rapl`) whose name matches `pred`.
pub fn find_zone(fs: &dyn Sysfs, base: &Path, control: &str, pred: impl Fn(&str) -> bool) -> Option<(String, PathBuf, String)> {
    let mut entries = fs.list(base).ok()?;
    entries.sort();
    for e in entries {
        // `intel-rapl:0` but not `intel-rapl:0:1` (subzone) nor `intel-rapl-mmio:0`.
        let Some(idx) = e.strip_prefix(control).and_then(|r| r.strip_prefix(':')) else { continue };
        if idx.contains(':') {
            continue;
        }
        let dir = base.join(&e);
        let name = fs.read_trimmed(&dir.join("name")).unwrap_or_default();
        if pred(&name) {
            return Some((e, dir, name));
        }
    }
    None
}

pub fn find_package_zone(fs: &dyn Sysfs, base: &Path, control: &str) -> Result<RaplZone, RaplError> {
    if fs.list(base).is_err() {
        return Err(RaplError::NoPowercap(base.display().to_string()));
    }
    let (id, dir, name) =
        find_zone(fs, base, control, |n| n.starts_with("package")).ok_or_else(|| RaplError::NoPackageZone(control.to_string()))?;
    let mut pl1 = None;
    let mut pl2 = None;
    for index in 0..8 {
        let Ok(cname) = fs.read_trimmed(&dir.join(format!("constraint_{index}_name"))) else { continue };
        let c = Constraint { index, dir: dir.clone() };
        match cname.as_str() {
            "long_term" => pl1 = Some(c),
            "short_term" => pl2 = Some(c),
            _ => {}
        }
    }
    let pl1 = pl1.ok_or_else(|| RaplError::NoLongTerm(id.clone()))?;
    Ok(RaplZone { id, dir, name, pl1, pl2 })
}

/// How a single written value ended up in hardware.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub enum Outcome {
    /// Read back exactly as requested.
    Exact,
    /// Hardware quantized the value (normal for tau).
    Rounded,
    /// Write succeeded but hardware holds a materially different value.
    Mismatch,
    /// The write itself failed.
    Rejected(String),
    /// This machine has no such control.
    Unsupported,
}

impl Outcome {
    pub fn is_ok(&self) -> bool {
        matches!(self, Outcome::Exact | Outcome::Rounded)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldReport {
    pub requested: f64,
    pub effective: Option<f64>,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApplyReport {
    pub event: String,
    pub reason: String,
    pub profile: ProfileId,
    pub at_unix: u64,
    pub pl1: FieldReport,
    pub pl2: FieldReport,
    pub tau: FieldReport,
    pub effective: Option<HwLimits>,
    pub success: bool,
}

/// Power limits are quantized to 1/8 W on Intel; anything within that is "rounded".
const POWER_ROUND_W: f64 = 0.13;
/// Time windows are encoded as 2^Y * (1 + Z/4) units, so quantization can be
/// up to ~12.5% relative.
const TAU_ROUND_REL: f64 = 0.15;

fn classify_power(req: f64, eff: f64) -> Outcome {
    let d = (eff - req).abs();
    if d < 0.0005 {
        Outcome::Exact
    } else if d <= POWER_ROUND_W {
        Outcome::Rounded
    } else {
        Outcome::Mismatch
    }
}

fn classify_tau(req: f64, eff: f64) -> Outcome {
    let rel = (eff - req).abs() / req.max(1e-9);
    if rel < 1e-6 {
        Outcome::Exact
    } else if rel <= TAU_ROUND_REL {
        Outcome::Rounded
    } else {
        Outcome::Mismatch
    }
}

impl RaplZone {
    pub fn read_limits(&self, fs: &dyn Sysfs) -> Result<HwLimits, RaplError> {
        let p1 = self.pl1.power_path();
        let w1 = self.pl1.window_path();
        let pl1_w = uw_to_w(fs.read_u64(&p1).map_err(io_err(&p1))?);
        let tau_s = us_to_s(fs.read_u64(&w1).map_err(io_err(&w1))?);
        let pl2_w = match &self.pl2 {
            Some(c) => {
                let p = c.power_path();
                Some(uw_to_w(fs.read_u64(&p).map_err(io_err(&p))?))
            }
            None => None,
        };
        Ok(HwLimits { pl1_w, pl2_w, tau_s })
    }

    /// Hardware-reported maximum for a constraint. Many firmwares report 0,
    /// meaning "unknown".
    pub fn max_w(fs: &dyn Sysfs, c: &Constraint) -> Option<f64> {
        fs.read_u64(&c.max_power_path()).ok().filter(|&v| v > 0).map(uw_to_w)
    }

    /// Write PL1, PL2, tau; read everything back and classify each field.
    pub fn apply(&self, fs: &dyn Sysfs, profile: ProfileId, req: &Limits, reason: &str) -> ApplyReport {
        let before = self.read_limits(fs).ok();
        let write = |path: PathBuf, v: u64| fs.write(&path, &v.to_string()).map_err(|e| e.to_string());

        // Keep PL1 <= PL2 at every intermediate step: when raising PL1 above
        // the current PL2, raise PL2 first.
        let pl2_first = matches!(before, Some(HwLimits { pl2_w: Some(cur), .. }) if req.pl1_w > cur);
        let mut pl1_res = Ok(());
        let mut pl2_res = Ok(());
        let do_pl1 = |res: &mut Result<(), String>| *res = write(self.pl1.power_path(), w_to_uw(req.pl1_w));
        let do_pl2 = |res: &mut Result<(), String>| {
            if let Some(c) = &self.pl2 {
                *res = write(c.power_path(), w_to_uw(req.pl2_w));
            }
        };
        if pl2_first {
            do_pl2(&mut pl2_res);
            do_pl1(&mut pl1_res);
        } else {
            do_pl1(&mut pl1_res);
            do_pl2(&mut pl2_res);
        }
        let tau_res = write(self.pl1.window_path(), s_to_us(req.tau_s));

        let after = self.read_limits(fs).ok();
        let field = |requested: f64, res: Result<(), String>, eff: Option<f64>, classify: fn(f64, f64) -> Outcome| {
            let outcome = match (res, eff) {
                (Err(e), _) => Outcome::Rejected(e),
                (Ok(()), Some(e)) => classify(requested, e),
                (Ok(()), None) => Outcome::Rejected("could not read back".into()),
            };
            FieldReport { requested, effective: eff, outcome }
        };
        let pl1 = field(req.pl1_w, pl1_res, after.map(|a| a.pl1_w), classify_power);
        let pl2 = if self.pl2.is_some() {
            field(req.pl2_w, pl2_res, after.and_then(|a| a.pl2_w), classify_power)
        } else {
            FieldReport { requested: req.pl2_w, effective: None, outcome: Outcome::Unsupported }
        };
        let tau = field(req.tau_s, tau_res, after.map(|a| a.tau_s), classify_tau);
        let success = pl1.outcome.is_ok() && tau.outcome.is_ok() && (pl2.outcome.is_ok() || pl2.outcome == Outcome::Unsupported);

        ApplyReport {
            event: "rapl_apply".into(),
            reason: reason.into(),
            profile,
            at_unix: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
            pl1,
            pl2,
            tau,
            effective: after,
            success,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::sysfs::MockSysfs;
    use std::io;

    pub const BASE: &str = "/sys/class/powercap";

    /// A tree modelled on the X1 Nano Gen 3.
    pub fn x1_nano() -> MockSysfs {
        let fs = MockSysfs::new();
        let z = format!("{BASE}/intel-rapl:0");
        fs.set(format!("{z}/name"), "package-0\n")
            .set(format!("{z}/constraint_0_name"), "long_term\n")
            .set(format!("{z}/constraint_0_power_limit_uw"), "6000000\n")
            .set(format!("{z}/constraint_0_time_window_us"), "2998272\n")
            .set(format!("{z}/constraint_0_max_power_uw"), "28000000\n")
            .set(format!("{z}/constraint_1_name"), "short_term\n")
            .set(format!("{z}/constraint_1_power_limit_uw"), "12000000\n")
            .set(format!("{z}/constraint_1_time_window_us"), "2440\n")
            .set(format!("{z}/constraint_1_max_power_uw"), "0\n")
            .set(format!("{z}/constraint_2_name"), "peak_power\n")
            .set(format!("{z}/constraint_2_power_limit_uw"), "140000000\n")
            .set(format!("{z}/energy_uj"), "1000000\n")
            .set(format!("{z}/max_energy_range_uj"), "262143328850\n")
            .set(format!("{BASE}/intel-rapl:0:0/name"), "core\n")
            .set(format!("{BASE}/intel-rapl:1/name"), "psys\n")
            .set(format!("{BASE}/intel-rapl:1/energy_uj"), "5000000\n")
            .set(format!("{BASE}/intel-rapl:1/max_energy_range_uj"), "262143328850\n")
            .set(format!("{BASE}/intel-rapl-mmio:0/name"), "package-0\n")
            .set(format!("{BASE}/intel-rapl-mmio:0/constraint_0_name"), "long_term\n")
            .set(format!("{BASE}/intel-rapl-mmio:0/constraint_0_power_limit_uw"), "28000000\n")
            .set(format!("{BASE}/intel-rapl-mmio:0/constraint_0_time_window_us"), "27983872\n")
            .set(format!("{BASE}/intel-rapl-mmio:0/constraint_1_name"), "short_term\n")
            .set(format!("{BASE}/intel-rapl-mmio:0/constraint_1_power_limit_uw"), "64000000\n");
        fs
    }

    fn zone(fs: &MockSysfs) -> RaplZone {
        find_package_zone(fs, Path::new(BASE), CONTROL_MSR).unwrap()
    }

    const BALANCED: Limits = Limits { pl1_w: 10.0, pl2_w: 20.0, tau_s: 8.0 };

    #[test]
    fn discovers_msr_package_not_mmio_or_subzone() {
        let fs = x1_nano();
        let z = zone(&fs);
        assert_eq!(z.id, "intel-rapl:0");
        assert_eq!(z.name, "package-0");
        assert_eq!(z.pl1.index, 0);
        assert_eq!(z.pl2.as_ref().unwrap().index, 1);
        let m = find_package_zone(&fs, Path::new(BASE), CONTROL_MMIO).unwrap();
        assert_eq!(m.id, "intel-rapl-mmio:0");
    }

    #[test]
    fn constraint_indexes_found_by_name() {
        let fs = MockSysfs::new();
        let z = format!("{BASE}/intel-rapl:3");
        fs.set(format!("{BASE}/intel-rapl:0/name"), "psys")
            .set(format!("{z}/name"), "package-0")
            .set(format!("{z}/constraint_0_name"), "short_term")
            .set(format!("{z}/constraint_0_power_limit_uw"), "1")
            .set(format!("{z}/constraint_1_name"), "long_term")
            .set(format!("{z}/constraint_1_power_limit_uw"), "1")
            .set(format!("{z}/constraint_1_time_window_us"), "1");
        let z = find_package_zone(&fs, Path::new(BASE), CONTROL_MSR).unwrap();
        assert_eq!(z.id, "intel-rapl:3");
        assert_eq!(z.pl1.index, 1);
        assert_eq!(z.pl2.unwrap().index, 0);
    }

    #[test]
    fn no_powercap() {
        let fs = MockSysfs::new();
        assert!(matches!(find_package_zone(&fs, Path::new(BASE), CONTROL_MSR), Err(RaplError::NoPowercap(_))));
    }

    #[test]
    fn reads_limits_and_max() {
        let fs = x1_nano();
        let z = zone(&fs);
        let l = z.read_limits(&fs).unwrap();
        assert_eq!(l.pl1_w, 6.0);
        assert_eq!(l.pl2_w, Some(12.0));
        assert!((l.tau_s - 2.998272).abs() < 1e-9);
        assert_eq!(RaplZone::max_w(&fs, &z.pl1), Some(28.0));
        assert_eq!(RaplZone::max_w(&fs, z.pl2.as_ref().unwrap()), None);
    }

    #[test]
    fn apply_exact() {
        let fs = x1_nano();
        let r = zone(&fs).apply(&fs, ProfileId::Balanced, &BALANCED, "test");
        assert!(r.success, "{r:?}");
        assert_eq!(r.pl1.outcome, Outcome::Exact);
        assert_eq!(r.pl2.outcome, Outcome::Exact);
        assert_eq!(r.tau.outcome, Outcome::Exact);
        assert_eq!(fs.get(format!("{BASE}/intel-rapl:0/constraint_0_power_limit_uw")).unwrap(), "10000000");
        assert_eq!(fs.get(format!("{BASE}/intel-rapl:0/constraint_0_time_window_us")).unwrap(), "8000000");
    }

    #[test]
    fn apply_rounded_tau() {
        let fs = x1_nano();
        // Firmware quantizes 8 s to 7.995392 s.
        fs.on_write(format!("{BASE}/intel-rapl:0/constraint_0_time_window_us"), |_| Ok("7995392".into()));
        let r = zone(&fs).apply(&fs, ProfileId::Balanced, &BALANCED, "test");
        assert!(r.success);
        assert_eq!(r.tau.outcome, Outcome::Rounded);
        assert!((r.tau.effective.unwrap() - 7.995392).abs() < 1e-9);
    }

    #[test]
    fn apply_clamped_pl2_is_mismatch() {
        let fs = x1_nano();
        fs.on_write(format!("{BASE}/intel-rapl:0/constraint_1_power_limit_uw"), |v| {
            Ok(v.parse::<u64>().unwrap().min(45_000_000).to_string())
        });
        let req = Limits { pl1_w: 28.0, pl2_w: 64.0, tau_s: 28.0 };
        let r = zone(&fs).apply(&fs, ProfileId::Performance, &req, "test");
        assert!(!r.success);
        assert_eq!(r.pl2.outcome, Outcome::Mismatch);
        assert_eq!(r.pl2.effective, Some(45.0));
    }

    #[test]
    fn apply_rejected_write() {
        let fs = x1_nano();
        fs.on_write(format!("{BASE}/intel-rapl:0/constraint_0_power_limit_uw"), |_| {
            Err(io::Error::new(io::ErrorKind::InvalidInput, "Invalid argument"))
        });
        let r = zone(&fs).apply(&fs, ProfileId::Balanced, &BALANCED, "test");
        assert!(!r.success);
        assert!(matches!(r.pl1.outcome, Outcome::Rejected(_)));
        assert_eq!(r.pl1.effective, Some(6.0));
    }

    #[test]
    fn apply_read_only() {
        let fs = x1_nano();
        fs.set_read_only(format!("{BASE}/intel-rapl:0/constraint_0_power_limit_uw"));
        let r = zone(&fs).apply(&fs, ProfileId::Balanced, &BALANCED, "test");
        assert!(!r.success);
        assert!(matches!(r.pl1.outcome, Outcome::Rejected(_)));
    }

    #[test]
    fn missing_pl2() {
        let fs = MockSysfs::new();
        let z = format!("{BASE}/intel-rapl:0");
        fs.set(format!("{z}/name"), "package-0")
            .set(format!("{z}/constraint_0_name"), "long_term")
            .set(format!("{z}/constraint_0_power_limit_uw"), "15000000")
            .set(format!("{z}/constraint_0_time_window_us"), "28000000");
        let zone = find_package_zone(&fs, Path::new(BASE), CONTROL_MSR).unwrap();
        assert!(zone.pl2.is_none());
        let r = zone.apply(&fs, ProfileId::Balanced, &BALANCED, "test");
        assert!(r.success);
        assert_eq!(r.pl2.outcome, Outcome::Unsupported);
    }

    #[test]
    fn raising_pl1_above_current_pl2_writes_pl2_first() {
        let fs = x1_nano();
        let pl2_path = format!("{BASE}/intel-rapl:0/constraint_1_power_limit_uw");
        let pl2_check = pl2_path.clone();
        // Simulate firmware refusing PL1 > PL2.
        let fs = std::sync::Arc::new(fs);
        let fs2 = fs.clone();
        fs.on_write(format!("{BASE}/intel-rapl:0/constraint_0_power_limit_uw"), move |v| {
            let pl2: u64 = fs2.get(&pl2_check).unwrap().trim().parse().unwrap();
            let v: u64 = v.parse().unwrap();
            if v > pl2 { Err(io::Error::new(io::ErrorKind::InvalidInput, "PL1 > PL2")) } else { Ok(v.to_string()) }
        });
        let req = Limits { pl1_w: 28.0, pl2_w: 64.0, tau_s: 28.0 };
        let r = zone(&fs).apply(&*fs, ProfileId::Performance, &req, "test");
        assert!(r.success, "{r:?}");
        assert_eq!(fs.get(&pl2_path).unwrap(), "64000000");
    }

    #[test]
    fn drift_detection() {
        let a = HwLimits { pl1_w: 10.0, pl2_w: Some(20.0), tau_s: 7.995 };
        assert!(!a.drifted_from(&a));
        assert!(a.drifted_from(&HwLimits { pl1_w: 28.0, ..a }));
        assert!(a.drifted_from(&HwLimits { tau_s: 28.0, ..a }));
        assert!(!a.drifted_from(&HwLimits { tau_s: 7.996, ..a }));
    }
}
