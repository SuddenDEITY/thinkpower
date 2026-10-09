//! Shared core for ThinkPower: config, validation, RAPL access, monitoring.
//!
//! Used by the privileged daemon (`thinkpowerd`) and the unprivileged
//! GUI/CLI (`thinkpower`). Everything that touches hardware goes through the
//! [`sysfs::Sysfs`] trait so it can be tested against a mock tree.

pub mod caps;
pub mod config;
pub mod monitor;
pub mod platform;
pub mod rapl;
pub mod status;
pub mod sysfs;
pub mod units;
pub mod validate;

/// Well-known D-Bus name and object path of the daemon.
pub const DBUS_NAME: &str = "io.github.thinkpower.Daemon1";
pub const DBUS_PATH: &str = "/io/github/thinkpower/Daemon1";

/// Polkit action required to change the configuration.
pub const POLKIT_ACTION_CONFIGURE: &str = "io.github.thinkpower.configure";

/// Installed restore script, run through pkexec by the GUI.
pub const RESTORE_HELPER: &str = "/usr/local/libexec/thinkpower/uninstall.sh";

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
