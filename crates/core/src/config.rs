//! Versioned, Git-friendly configuration (TOML).

use serde::{Deserialize, Serialize};
use std::fmt;
use std::io;
use std::path::Path;

pub const CONFIG_VERSION: u32 = 1;
pub const SYSTEM_CONFIG_PATH: &str = "/etc/thinkpower/config.toml";

/// One of the three desktop power profiles exposed by power-profiles-daemon
/// (or tuned-ppd, which implements the same D-Bus API).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileId {
    PowerSave,
    Balanced,
    Performance,
}

impl ProfileId {
    pub const ALL: [ProfileId; 3] = [ProfileId::PowerSave, ProfileId::Balanced, ProfileId::Performance];

    /// Map a PPD `ActiveProfile` value to a profile.
    pub fn from_ppd(name: &str) -> Option<Self> {
        match name {
            "power-saver" => Some(Self::PowerSave),
            "balanced" => Some(Self::Balanced),
            "performance" => Some(Self::Performance),
            _ => None,
        }
    }

    pub fn ppd_name(self) -> &'static str {
        match self {
            Self::PowerSave => "power-saver",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::PowerSave => "power_save",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::PowerSave => "Power Save",
            Self::Balanced => "Balanced",
            Self::Performance => "Performance",
        }
    }

    /// Lenient parser for CLI input: `power-save`, `power_save`, `power-saver`, `powersave`, ...
    pub fn parse(s: &str) -> Option<Self> {
        let n: String = s.to_ascii_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        match n.as_str() {
            "powersave" | "powersaver" | "saver" | "eco" => Some(Self::PowerSave),
            "balanced" | "balance" => Some(Self::Balanced),
            "performance" | "perf" => Some(Self::Performance),
            _ => None,
        }
    }
}

impl fmt::Display for ProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Requested RAPL limits, in user units.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    /// Long-term (sustained) package power limit, watts.
    pub pl1_w: f64,
    /// Short-term (boost) package power limit, watts.
    pub pl2_w: f64,
    /// PL1 averaging time window, seconds.
    pub tau_s: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileConfig {
    pub pl1_w: f64,
    pub pl2_w: f64,
    pub tau_s: f64,
    /// Optional ACPI platform_profile override (e.g. "balanced"). When unset,
    /// whatever tuned chose for this profile is left alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform_profile: Option<String>,
}

impl ProfileConfig {
    pub fn limits(&self) -> Limits {
        Limits { pl1_w: self.pl1_w, pl2_w: self.pl2_w, tau_s: self.tau_s }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profiles {
    pub power_save: ProfileConfig,
    pub balanced: ProfileConfig,
    pub performance: ProfileConfig,
}

impl Profiles {
    pub fn get(&self, id: ProfileId) -> &ProfileConfig {
        match id {
            ProfileId::PowerSave => &self.power_save,
            ProfileId::Balanced => &self.balanced,
            ProfileId::Performance => &self.performance,
        }
    }

    pub fn get_mut(&mut self, id: ProfileId) -> &mut ProfileConfig {
        match id {
            ProfileId::PowerSave => &mut self.power_save,
            ProfileId::Balanced => &mut self.balanced,
            ProfileId::Performance => &mut self.performance,
        }
    }
}

/// Informational: which machine the config was tuned on.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Machine {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<Machine>,
    pub profiles: Profiles,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Io { path: String, source: io::Error },
    #[error("invalid config: {0}")]
    Parse(String),
    #[error("unsupported config version {0} (this build understands version {CONFIG_VERSION})")]
    Version(u32),
}

impl Config {
    pub fn from_toml(s: &str) -> Result<Self, ConfigError> {
        let cfg: Config = toml::from_str(s).map_err(|e| ConfigError::Parse(e.to_string()))?;
        if cfg.version != CONFIG_VERSION {
            return Err(ConfigError::Version(cfg.version));
        }
        Ok(cfg)
    }

    pub fn to_toml(&self) -> String {
        let body = toml::to_string_pretty(self).expect("config is always serializable");
        format!("# ThinkPower configuration. Units: watts (pl1_w, pl2_w), seconds (tau_s).\n{body}")
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let s = std::fs::read_to_string(path)
            .map_err(|source| ConfigError::Io { path: path.display().to_string(), source })?;
        Self::from_toml(&s)
    }

    /// Write atomically (temp file + rename) with mode 0644.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let dir = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(".config.toml.tmp");
        {
            let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o644).open(&tmp)?;
            f.write_all(self.to_toml().as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../../configs/x1-nano-gen3.toml");

    #[test]
    fn parses_fixture() {
        let c = Config::from_toml(FIXTURE).unwrap();
        assert_eq!(c.profiles.balanced.limits(), Limits { pl1_w: 10.0, pl2_w: 20.0, tau_s: 8.0 });
        assert_eq!(c.profiles.performance.pl2_w, 64.0);
        assert_eq!(c.profiles.performance.platform_profile.as_deref(), Some("balanced"));
        assert_eq!(c.profiles.power_save.platform_profile, None);
    }

    #[test]
    fn roundtrip() {
        let c = Config::from_toml(FIXTURE).unwrap();
        let again = Config::from_toml(&c.to_toml()).unwrap();
        assert_eq!(c, again);
    }

    #[test]
    fn integer_values_accepted() {
        let s = r#"
            version = 1
            [profiles.power_save]
            pl1_w = 5
            pl2_w = 10
            tau_s = 2
            [profiles.balanced]
            pl1_w = 10
            pl2_w = 20
            tau_s = 8
            [profiles.performance]
            pl1_w = 28
            pl2_w = 64
            tau_s = 28
        "#;
        // toml integers do not coerce to f64 automatically; make sure the error is clear.
        match Config::from_toml(s) {
            Ok(c) => assert_eq!(c.profiles.power_save.pl1_w, 5.0),
            Err(e) => panic!("integers should be accepted: {e}"),
        }
    }

    #[test]
    fn rejects_future_version() {
        let s = FIXTURE.replace("version = 1", "version = 2");
        assert!(matches!(Config::from_toml(&s), Err(ConfigError::Version(2))));
    }

    #[test]
    fn profile_parse() {
        assert_eq!(ProfileId::parse("power-save"), Some(ProfileId::PowerSave));
        assert_eq!(ProfileId::parse("power_saver"), Some(ProfileId::PowerSave));
        assert_eq!(ProfileId::parse("Performance"), Some(ProfileId::Performance));
        assert_eq!(ProfileId::from_ppd("power-saver"), Some(ProfileId::PowerSave));
        assert_eq!(ProfileId::parse("turbo"), None);
    }
}
