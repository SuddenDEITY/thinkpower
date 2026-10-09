//! Daemon status snapshot, sent to clients as JSON over D-Bus.

use crate::caps::Capabilities;
use crate::config::{Config, ProfileId};
use crate::monitor::{HistoryPoint, PowerReading};
use crate::rapl::{ApplyReport, HwLimits};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub daemon_version: String,
    pub config_path: String,
    pub config: Option<Config>,
    pub config_error: Option<String>,
    pub capabilities: Capabilities,
    /// Raw PPD `ActiveProfile` (e.g. "power-saver").
    pub ppd_profile: Option<String>,
    pub active: Option<ProfileId>,
    /// Current MSR RAPL limits (what we write).
    pub effective: Option<HwLimits>,
    /// Current firmware MMIO limits; the CPU enforces the lower of both.
    pub firmware_mmio: Option<HwLimits>,
    pub platform_profile: Option<String>,
    pub last_apply: Option<ApplyReport>,
    pub power: PowerReading,
    pub history: Vec<HistoryPoint>,
    /// Non-fatal problems worth showing (e.g. PPD not reachable).
    pub warnings: Vec<String>,
}
