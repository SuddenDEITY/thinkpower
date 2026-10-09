//! Blocking D-Bus client for thinkpowerd.

use thinkpower_core::config::Config;
use thinkpower_core::status::Status;
use zbus::proxy;

#[proxy(
    interface = "io.github.thinkpower.Daemon1",
    default_service = "io.github.thinkpower.Daemon1",
    default_path = "/io/github/thinkpower/Daemon1",
    gen_async = false,
    gen_blocking = true,
    blocking_name = "DaemonProxy"
)]
trait Daemon {
    fn get_status(&self) -> zbus::Result<String>;
    fn set_config(&self, config_toml: &str) -> zbus::Result<String>;
    fn reapply(&self) -> zbus::Result<String>;
}

pub struct Client {
    proxy: DaemonProxy<'static>,
}

fn parse(json: String) -> Result<Status, String> {
    serde_json::from_str(&json).map_err(|e| format!("bad status from daemon: {e}"))
}

/// Turn D-Bus errors into something a user can act on.
fn explain(e: zbus::Error) -> String {
    match &e {
        zbus::Error::MethodError(name, msg, _) => {
            let msg = msg.clone().unwrap_or_default();
            match name.as_str() {
                "org.freedesktop.DBus.Error.ServiceUnknown" | "org.freedesktop.DBus.Error.NameHasNoOwner" => {
                    "thinkpowerd is not running. Is ThinkPower installed? (systemctl status thinkpowerd)".into()
                }
                "org.freedesktop.DBus.Error.AccessDenied" => format!("Not authorized: {msg}"),
                "org.freedesktop.DBus.Error.InvalidArgs" => format!("Rejected by daemon:\n{msg}"),
                _ => format!("{name}: {msg}"),
            }
        }
        _ => e.to_string(),
    }
}

impl Client {
    pub fn connect() -> Result<Self, String> {
        let conn = zbus::blocking::connection::Builder::system()
            .and_then(|b| b.method_timeout(std::time::Duration::from_secs(300)).build())
            .map_err(|e| format!("cannot connect to the system bus: {e}"))?;
        let proxy = DaemonProxy::new(&conn).map_err(explain)?;
        Ok(Client { proxy })
    }

    pub fn status(&self) -> Result<Status, String> {
        parse(self.proxy.get_status().map_err(explain)?)
    }

    /// Send a new config. May block while the user answers a polkit prompt.
    pub fn set_config(&self, cfg: &Config) -> Result<Status, String> {
        parse(self.proxy.set_config(&cfg.to_toml()).map_err(explain)?)
    }

    pub fn reapply(&self) -> Result<Status, String> {
        parse(self.proxy.reapply().map_err(explain)?)
    }
}
