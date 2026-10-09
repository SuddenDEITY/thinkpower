//! ACPI platform profile (`/sys/firmware/acpi/platform_profile`).
//!
//! This is a separate control layer from RAPL and from the tuned profile;
//! tuned's stock profiles set it themselves.

use crate::sysfs::Sysfs;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const PATH: &str = "/sys/firmware/acpi/platform_profile";
pub const CHOICES_PATH: &str = "/sys/firmware/acpi/platform_profile_choices";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlatformProfileInfo {
    pub current: String,
    pub choices: Vec<String>,
}

pub fn read(fs: &dyn Sysfs) -> Option<PlatformProfileInfo> {
    let current = fs.read_trimmed(Path::new(PATH)).ok()?;
    let choices = fs
        .read_trimmed(Path::new(CHOICES_PATH))
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    Some(PlatformProfileInfo { current, choices })
}

pub fn current(fs: &dyn Sysfs) -> Option<String> {
    fs.read_trimmed(Path::new(PATH)).ok()
}

pub fn write(fs: &dyn Sysfs, value: &str) -> std::io::Result<()> {
    fs.write(Path::new(PATH), value)
}
