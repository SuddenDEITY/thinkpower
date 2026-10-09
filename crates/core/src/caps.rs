//! Hardware / OS capability discovery.

use crate::platform::{self, PlatformProfileInfo};
use crate::rapl::{self, HwLimits, RaplZone};
use crate::sysfs::Sysfs;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneInfo {
    pub id: String,
    pub name: String,
    pub path: String,
    pub pl1_writable: bool,
    pub tau_writable: bool,
    pub pl2_present: bool,
    pub pl2_writable: bool,
    pub pl1_max_w: Option<f64>,
    pub pl2_max_w: Option<f64>,
    pub current: Option<HwLimits>,
}

impl ZoneInfo {
    pub fn from_zone(fs: &dyn Sysfs, z: &RaplZone) -> Self {
        ZoneInfo {
            id: z.id.clone(),
            name: z.name.clone(),
            path: z.dir.display().to_string(),
            pl1_writable: fs.owner_writable(&z.pl1.power_path()),
            tau_writable: fs.owner_writable(&z.pl1.window_path()),
            pl2_present: z.pl2.is_some(),
            pl2_writable: z.pl2.as_ref().is_some_and(|c| fs.owner_writable(&c.power_path())),
            pl1_max_w: RaplZone::max_w(fs, &z.pl1),
            pl2_max_w: z.pl2.as_ref().and_then(|c| RaplZone::max_w(fs, c)),
            current: z.read_limits(fs).ok(),
        }
    }

    pub fn fully_writable(&self) -> bool {
        self.pl1_writable && self.tau_writable && (!self.pl2_present || self.pl2_writable)
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Capabilities {
    pub cpu_vendor: String,
    pub cpu_model: String,
    pub machine_model: String,
    pub os: String,
    pub kernel: String,
    pub rapl: Option<ZoneInfo>,
    pub rapl_error: Option<String>,
    /// Firmware-controlled MMIO limits, shown for information only.
    pub mmio: Option<ZoneInfo>,
    pub platform_profile: Option<PlatformProfileInfo>,
}

impl Capabilities {
    /// Human-readable reason why custom limits cannot work here, if any.
    pub fn blocking_problem(&self) -> Option<String> {
        if self.cpu_vendor != "GenuineIntel" && !self.cpu_vendor.is_empty() {
            return Some(format!("CPU vendor {} is not supported (Intel RAPL only).", self.cpu_vendor));
        }
        match &self.rapl {
            None => Some(self.rapl_error.clone().unwrap_or_else(|| "Intel RAPL not found.".into())),
            Some(z) if !z.fully_writable() => Some(
                "Intel RAPL is present but read-only.\nThis laptop cannot currently use custom package power limits.".into(),
            ),
            _ => None,
        }
    }
}

fn cpuinfo_field(cpuinfo: &str, key: &str) -> String {
    cpuinfo
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            (k.trim() == key).then(|| v.trim().to_string())
        })
        .unwrap_or_default()
}

fn os_pretty_name(fs: &dyn Sysfs) -> String {
    fs.read(Path::new("/etc/os-release"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string()))
        })
        .unwrap_or_default()
}

/// The DMI model string. Lenovo puts the marketing name in `product_version`.
pub fn machine_model(fs: &dyn Sysfs) -> String {
    let version = fs.read_trimmed(Path::new("/sys/class/dmi/id/product_version")).unwrap_or_default();
    if !version.is_empty() && version != "None" && !version.starts_with("To be filled") {
        return version;
    }
    fs.read_trimmed(Path::new("/sys/class/dmi/id/product_name")).unwrap_or_default()
}

pub fn discover(fs: &dyn Sysfs) -> Capabilities {
    let base = Path::new(rapl::POWERCAP);
    let cpuinfo = fs.read(Path::new("/proc/cpuinfo")).unwrap_or_default();
    let (rapl, rapl_error) = match rapl::find_package_zone(fs, base, rapl::CONTROL_MSR) {
        Ok(z) => (Some(ZoneInfo::from_zone(fs, &z)), None),
        Err(e) => (None, Some(e.to_string())),
    };
    let mmio = rapl::find_package_zone(fs, base, rapl::CONTROL_MMIO).ok().map(|z| ZoneInfo::from_zone(fs, &z));
    Capabilities {
        cpu_vendor: cpuinfo_field(&cpuinfo, "vendor_id"),
        cpu_model: cpuinfo_field(&cpuinfo, "model name"),
        machine_model: machine_model(fs),
        os: os_pretty_name(fs),
        kernel: fs.read_trimmed(Path::new("/proc/sys/kernel/osrelease")).unwrap_or_default(),
        rapl,
        rapl_error,
        mmio,
        platform_profile: platform::read(fs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rapl::tests::x1_nano;

    #[test]
    fn discover_x1() {
        let fs = x1_nano();
        fs.set("/proc/cpuinfo", "vendor_id\t: GenuineIntel\nmodel name\t: 13th Gen Intel(R) Core(TM) i7-1370P\n")
            .set(platform::PATH, "balanced\n")
            .set(platform::CHOICES_PATH, "low-power balanced performance\n")
            .set("/sys/class/dmi/id/product_version", "ThinkPad X1 Nano Gen 3\n");
        let c = discover(&fs);
        assert_eq!(c.cpu_model, "13th Gen Intel(R) Core(TM) i7-1370P");
        assert_eq!(c.machine_model, "ThinkPad X1 Nano Gen 3");
        let z = c.rapl.as_ref().unwrap();
        assert!(z.fully_writable());
        assert_eq!(z.pl1_max_w, Some(28.0));
        assert_eq!(c.mmio.as_ref().unwrap().current.unwrap().pl1_w, 28.0);
        assert_eq!(c.platform_profile.as_ref().unwrap().choices.len(), 3);
        assert!(c.blocking_problem().is_none());
    }

    #[test]
    fn read_only_rapl_is_blocking() {
        let fs = x1_nano();
        fs.set_read_only("/sys/class/powercap/intel-rapl:0/constraint_0_power_limit_uw");
        let c = discover(&fs);
        assert!(c.blocking_problem().unwrap().contains("read-only"));
    }
}
