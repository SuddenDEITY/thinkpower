//! Config validation, including hardware compatibility checks.

use crate::caps::Capabilities;
use crate::config::{Config, ProfileId};
use serde::{Deserialize, Serialize};

/// Absolute sanity bounds, applied even when hardware reports no maximum.
pub const MAX_SANE_W: f64 = 250.0;
pub const MIN_TAU_S: f64 = 0.01;
pub const MAX_TAU_S: f64 = 448.0;
/// Below this the machine tends to become unusable.
pub const LOW_PL1_WARN_W: f64 = 3.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    Pl1,
    Pl2,
    Tau,
    PlatformProfile,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Issue {
    pub profile: ProfileId,
    pub field: Field,
    pub severity: Severity,
    pub message: String,
    /// A value the user can accept with one click (e.g. the hardware max).
    pub suggested: Option<f64>,
}

pub fn has_errors(issues: &[Issue]) -> bool {
    issues.iter().any(|i| i.severity == Severity::Error)
}

/// Accept an issue's suggested value (e.g. "use the 45 W hardware max").
/// Returns false if the issue has no suggestion.
pub fn apply_suggestion(cfg: &mut Config, issue: &Issue) -> bool {
    let Some(v) = issue.suggested else { return false };
    let p = cfg.profiles.get_mut(issue.profile);
    match issue.field {
        Field::Pl1 => p.pl1_w = v,
        Field::Pl2 => p.pl2_w = v,
        Field::Tau => p.tau_s = v,
        Field::PlatformProfile => return false,
    }
    true
}

/// Validate `cfg`. With `caps`, also check against hardware-reported limits.
/// Values above a hardware maximum are reported, never silently clamped.
pub fn validate(cfg: &Config, caps: Option<&Capabilities>) -> Vec<Issue> {
    let mut out = Vec::new();
    let zone = caps.and_then(|c| c.rapl.as_ref());
    for id in ProfileId::ALL {
        let p = cfg.profiles.get(id);
        let mut push = |field, severity, message: String, suggested| out.push(Issue { profile: id, field, severity, message, suggested });

        for (field, name, v) in [(Field::Pl1, "PL1", p.pl1_w), (Field::Pl2, "PL2", p.pl2_w)] {
            if !v.is_finite() || v <= 0.0 {
                push(field, Severity::Error, format!("{name} must be greater than 0 W."), None);
            } else if v > MAX_SANE_W {
                push(field, Severity::Error, format!("{name} {v} W is not a sensible package limit."), None);
            }
        }
        if !p.tau_s.is_finite() || p.tau_s < MIN_TAU_S || p.tau_s > MAX_TAU_S {
            push(Field::Tau, Severity::Error, format!("Tau must be between {MIN_TAU_S} and {MAX_TAU_S} s."), None);
        }
        if p.pl1_w > p.pl2_w {
            push(Field::Pl1, Severity::Error, "PL1 must not exceed PL2.".into(), Some(p.pl2_w));
        }
        if p.pl1_w > 0.0 && p.pl1_w < LOW_PL1_WARN_W {
            push(Field::Pl1, Severity::Warning, format!("PL1 below {LOW_PL1_WARN_W} W may make the system very sluggish."), None);
        }

        if let Some(z) = zone {
            if let Some(max) = z.pl1_max_w.filter(|&m| p.pl1_w > m) {
                push(Field::Pl1, Severity::Error, format!("PL1 {} W exceeds the hardware maximum of {max} W.", p.pl1_w), Some(max));
            }
            if let Some(max) = z.pl2_max_w.filter(|&m| p.pl2_w > m) {
                push(Field::Pl2, Severity::Error, format!("PL2 {} W exceeds the hardware maximum of {max} W.", p.pl2_w), Some(max));
            }
            if !z.pl2_present {
                push(Field::Pl2, Severity::Warning, "This CPU exposes no PL2 control; PL2 will be ignored.".into(), None);
            }
        }

        if p.platform_profile.is_some() {
            push(
                Field::PlatformProfile,
                Severity::Warning,
                "platform_profile override is no longer supported (tuned-ppd switches the profile back); it is ignored and will be dropped on save.".into(),
                None,
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::ZoneInfo;
    use crate::platform::PlatformProfileInfo;

    fn cfg() -> Config {
        Config::from_toml(include_str!("../../../configs/x1-nano-gen3.toml")).unwrap()
    }

    fn caps(pl1_max: Option<f64>, pl2_max: Option<f64>) -> Capabilities {
        Capabilities {
            rapl: Some(ZoneInfo {
                id: "intel-rapl:0".into(),
                name: "package-0".into(),
                path: String::new(),
                pl1_writable: true,
                tau_writable: true,
                pl2_present: true,
                pl2_writable: true,
                pl1_max_w: pl1_max,
                pl2_max_w: pl2_max,
                current: None,
            }),
            platform_profile: Some(PlatformProfileInfo {
                current: "balanced".into(),
                choices: vec!["low-power".into(), "balanced".into(), "performance".into()],
            }),
            ..Default::default()
        }
    }

    #[test]
    fn fixture_is_valid_on_x1() {
        let issues = validate(&cfg(), Some(&caps(Some(28.0), None)));
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn basic_rules() {
        let mut c = cfg();
        c.profiles.balanced.pl1_w = 0.0;
        c.profiles.power_save.pl1_w = 30.0; // > PL2 12
        c.profiles.performance.tau_s = -1.0;
        let issues = validate(&c, None);
        assert!(issues.iter().any(|i| i.profile == ProfileId::Balanced && i.field == Field::Pl1));
        assert!(issues.iter().any(|i| i.profile == ProfileId::PowerSave && i.message.contains("exceed PL2")));
        assert!(issues.iter().any(|i| i.profile == ProfileId::Performance && i.field == Field::Tau));
        assert!(has_errors(&issues));
    }

    #[test]
    fn above_hardware_max_is_reported_with_suggestion() {
        let issues = validate(&cfg(), Some(&caps(Some(28.0), Some(45.0))));
        let i = issues.iter().find(|i| i.profile == ProfileId::Performance && i.field == Field::Pl2).unwrap();
        assert_eq!(i.severity, Severity::Error);
        assert_eq!(i.suggested, Some(45.0));
    }

    #[test]
    fn legacy_platform_profile_only_warns() {
        let mut c = cfg();
        c.profiles.performance.platform_profile = Some("balanced".into());
        let issues = validate(&c, Some(&caps(None, None)));
        assert!(issues.iter().any(|i| i.field == Field::PlatformProfile && i.severity == Severity::Warning));
        assert!(!has_errors(&issues));
    }

    #[test]
    fn low_pl1_warns() {
        let mut c = cfg();
        c.profiles.power_save.pl1_w = 2.0;
        let issues = validate(&c, None);
        assert!(!has_errors(&issues));
        assert!(issues.iter().any(|i| i.severity == Severity::Warning));
    }
}
