//! thinkpowerd: system daemon that binds RAPL limits to desktop power profiles.
//!
//! - follows the PPD `ActiveProfile` (served by tuned-ppd or power-profiles-daemon)
//!   and applies the configured PL1/PL2/tau for it, verifying by readback;
//! - re-checks after resume and periodically, reapplying only on drift;
//! - samples package/platform power and publishes it with the status;
//! - accepts new configs from clients, authorized through polkit.

use futures_util::StreamExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thinkpower_core::caps::{self, Capabilities};
use thinkpower_core::config::{Config, ProfileId, SYSTEM_CONFIG_PATH};
use thinkpower_core::monitor::PowerMonitor;
use thinkpower_core::platform::{self};
use thinkpower_core::rapl::{self, ApplyReport, PlatformReport, RaplZone};
use thinkpower_core::status::Status;
use thinkpower_core::sysfs::{RealSysfs, Sysfs};
use thinkpower_core::validate::{self, Severity};
use thinkpower_core::{DBUS_NAME, DBUS_PATH, POLKIT_ACTION_CONFIGURE};
use zbus::object_server::SignalEmitter;
use zbus::{fdo, interface, proxy};

const RECONCILE_EVERY: Duration = Duration::from_secs(10);
const SAMPLE_EVERY: Duration = Duration::from_secs(1);
/// tuned applies its own profile (including platform_profile) around the time
/// PPD reports the switch; check again shortly after so our override wins.
const POST_SWITCH_RECHECK: Duration = Duration::from_secs(3);
const POST_RESUME_DELAY: Duration = Duration::from_secs(2);

#[proxy(
    interface = "org.freedesktop.UPower.PowerProfiles",
    default_service = "org.freedesktop.UPower.PowerProfiles",
    default_path = "/org/freedesktop/UPower/PowerProfiles"
)]
trait PowerProfiles {
    #[zbus(property)]
    fn active_profile(&self) -> zbus::Result<String>;
}

#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait Login1Manager {
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

/// Structured log line (JSON) to stderr, which systemd sends to the journal.
fn log(event: &str, fields: serde_json::Value) {
    let mut obj = serde_json::json!({ "event": event });
    if let (Some(o), serde_json::Value::Object(f)) = (obj.as_object_mut(), fields) {
        o.extend(f);
    }
    eprintln!("{obj}");
}

struct State {
    fs: Box<dyn Sysfs>,
    config_path: PathBuf,
    config: Option<Config>,
    config_error: Option<String>,
    caps: Capabilities,
    zone: Option<RaplZone>,
    mmio: Option<RaplZone>,
    ppd_profile: Option<String>,
    last_apply: Option<ApplyReport>,
    monitor: PowerMonitor,
    power: thinkpower_core::monitor::PowerReading,
    started: Instant,
    warnings: Vec<String>,
}

type Shared = Arc<Mutex<State>>;

impl State {
    fn new(config_path: PathBuf) -> Self {
        let fs: Box<dyn Sysfs> = Box::new(RealSysfs);
        let caps = caps::discover(&*fs);
        let base = std::path::Path::new(rapl::POWERCAP);
        let zone = rapl::find_package_zone(&*fs, base, rapl::CONTROL_MSR).ok();
        let mmio = rapl::find_package_zone(&*fs, base, rapl::CONTROL_MMIO).ok();
        let monitor = PowerMonitor::discover(&*fs);
        let mut s = State {
            fs,
            config_path,
            config: None,
            config_error: None,
            caps,
            zone,
            mmio,
            ppd_profile: None,
            last_apply: None,
            monitor,
            power: Default::default(),
            started: Instant::now(),
            warnings: Vec::new(),
        };
        s.load_config();
        log("startup", serde_json::json!({ "capabilities": &s.caps, "config_error": &s.config_error }));
        s
    }

    fn load_config(&mut self) {
        match Config::load(&self.config_path) {
            Ok(cfg) => self.set_config(cfg),
            Err(e) => {
                self.config = None;
                self.config_error = Some(e.to_string());
            }
        }
    }

    /// Accept a config only if it has no validation errors against this hardware.
    fn set_config(&mut self, cfg: Config) {
        let errors: Vec<String> = validate::validate(&cfg, Some(&self.caps))
            .into_iter()
            .filter(|i| i.severity == Severity::Error)
            .map(|i| format!("{}: {}", i.profile, i.message))
            .collect();
        if errors.is_empty() {
            self.config = Some(cfg);
            self.config_error = None;
        } else {
            self.config = Some(cfg);
            self.config_error = Some(format!("config not applied: {}", errors.join("; ")));
        }
    }

    fn active(&self) -> Option<ProfileId> {
        self.ppd_profile.as_deref().and_then(ProfileId::from_ppd)
    }

    /// Apply the configured limits for the active profile unconditionally.
    fn apply(&mut self, reason: &str) {
        let Some(profile) = self.active() else { return };
        if self.config_error.is_some() {
            return;
        }
        let (Some(cfg), Some(zone)) = (&self.config, &self.zone) else { return };
        let pc = cfg.profiles.get(profile).clone();
        let mut report = zone.apply(&*self.fs, profile, &pc.limits(), reason);
        if let Some(pp) = &pc.platform_profile {
            let error = platform::write(&*self.fs, pp).err().map(|e| e.to_string());
            let effective = platform::current(&*self.fs);
            if error.is_some() || effective.as_deref() != Some(pp) {
                report.success = false;
            }
            report.platform_profile = Some(PlatformReport { requested: pp.clone(), effective, error });
        }
        log("rapl_apply", serde_json::to_value(&report).unwrap_or_default());
        self.last_apply = Some(report);
    }

    /// Reapply only if the profile changed or hardware drifted from what we set.
    fn reconcile(&mut self, reason: &str) {
        let Some(profile) = self.active() else { return };
        let needs = match (&self.last_apply, &self.zone) {
            (None, _) => true,
            (Some(last), _) if last.profile != profile => true,
            (Some(last), Some(zone)) => {
                let rapl_drift = match (zone.read_limits(&*self.fs), &last.effective) {
                    (Ok(now), Some(prev)) => now.drifted_from(prev),
                    _ => true,
                };
                let pp_drift = last
                    .platform_profile
                    .as_ref()
                    .is_some_and(|p| platform::current(&*self.fs).as_deref() != Some(p.requested.as_str()));
                // Don't hammer firmware that keeps refusing: only retry on real drift.
                rapl_drift || pp_drift
            }
            _ => false,
        };
        if needs {
            self.apply(reason);
        }
    }

    fn sample(&mut self) {
        let t = self.started.elapsed().as_secs_f64();
        self.power = self.monitor.sample(&*self.fs, t);
    }

    fn status(&self) -> Status {
        Status {
            daemon_version: thinkpower_core::VERSION.into(),
            config_path: self.config_path.display().to_string(),
            config: self.config.clone(),
            config_error: self.config_error.clone(),
            capabilities: self.caps.clone(),
            ppd_profile: self.ppd_profile.clone(),
            active: self.active(),
            effective: self.zone.as_ref().and_then(|z| z.read_limits(&*self.fs).ok()),
            firmware_mmio: self.mmio.as_ref().and_then(|z| z.read_limits(&*self.fs).ok()),
            platform_profile: platform::current(&*self.fs),
            last_apply: self.last_apply.clone(),
            power: self.power.clone(),
            history: self.monitor.history(),
            warnings: self.warnings.clone(),
        }
    }
}

struct Daemon {
    state: Shared,
}

async fn authorize(conn: &zbus::Connection, sender: &str) -> fdo::Result<()> {
    use std::collections::HashMap;
    use zbus::zvariant::Value;
    let authority = zbus::Proxy::new(
        conn,
        "org.freedesktop.PolicyKit1",
        "/org/freedesktop/PolicyKit1/Authority",
        "org.freedesktop.PolicyKit1.Authority",
    )
    .await?;
    let mut subject_details: HashMap<&str, Value> = HashMap::new();
    subject_details.insert("name", Value::from(sender));
    let subject = ("system-bus-name", subject_details);
    let details: HashMap<&str, &str> = HashMap::new();
    const ALLOW_USER_INTERACTION: u32 = 1;
    let (authorized, _challenge, _details): (bool, bool, HashMap<String, String>) = authority
        .call("CheckAuthorization", &(subject, POLKIT_ACTION_CONFIGURE, details, ALLOW_USER_INTERACTION, ""))
        .await?;
    if authorized { Ok(()) } else { Err(fdo::Error::AccessDenied("not authorized".into())) }
}

#[interface(name = "io.github.thinkpower.Daemon1")]
impl Daemon {
    /// Full status as JSON. No authorization required.
    fn get_status(&self) -> String {
        let st = self.state.lock().unwrap();
        serde_json::to_string(&st.status()).unwrap_or_default()
    }

    /// Replace the configuration (TOML). Requires polkit authorization.
    /// Rejects configs with validation errors; applies the active profile.
    async fn set_config(
        &self,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        config_toml: String,
    ) -> fdo::Result<String> {
        let sender = hdr.sender().ok_or_else(|| fdo::Error::AccessDenied("no sender".into()))?.to_string();
        let cfg = Config::from_toml(&config_toml).map_err(|e| fdo::Error::InvalidArgs(e.to_string()))?;
        authorize(conn, &sender).await?;

        let status = {
            let mut st = self.state.lock().unwrap();
            let issues = validate::validate(&cfg, Some(&st.caps));
            let errors: Vec<String> =
                issues.iter().filter(|i| i.severity == Severity::Error).map(|i| format!("{}: {}", i.profile, i.message)).collect();
            if !errors.is_empty() {
                return Err(fdo::Error::InvalidArgs(errors.join("\n")));
            }
            let path = st.config_path.clone();
            cfg.save(&path).map_err(|e| fdo::Error::Failed(format!("cannot write {}: {e}", path.display())))?;
            log("config_saved", serde_json::json!({ "path": path, "by": sender }));
            st.set_config(cfg);
            st.apply("config-change");
            serde_json::to_string(&st.status()).unwrap_or_default()
        };
        let _ = Self::changed(&emitter).await;
        Ok(status)
    }

    /// Re-apply the active profile now. Only re-applies already configured values.
    fn reapply(&self) -> String {
        let mut st = self.state.lock().unwrap();
        st.apply("manual");
        serde_json::to_string(&st.status()).unwrap_or_default()
    }

    /// Emitted when the configuration changed.
    #[zbus(signal)]
    async fn changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

async fn watch_profiles(conn: zbus::Connection, state: Shared) {
    loop {
        if let Err(e) = follow_ppd(&conn, &state).await {
            let msg = format!("power profiles service unavailable: {e}");
            log("ppd_error", serde_json::json!({ "error": msg }));
            let mut st = state.lock().unwrap();
            if !st.warnings.contains(&msg) {
                st.warnings.push(msg);
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn follow_ppd(conn: &zbus::Connection, state: &Shared) -> zbus::Result<()> {
    let ppd = PowerProfilesProxy::new(conn).await?;
    let mut changes = ppd.receive_active_profile_changed().await;
    let initial = ppd.active_profile().await?;
    {
        let mut st = state.lock().unwrap();
        st.warnings.retain(|w| !w.starts_with("power profiles service"));
        st.ppd_profile = Some(initial.clone());
        log("profile", serde_json::json!({ "ppd_profile": initial, "reason": "startup" }));
        st.apply("startup");
    }
    while let Some(change) = changes.next().await {
        let name = change.get().await?;
        {
            let mut st = state.lock().unwrap();
            if st.ppd_profile.as_deref() == Some(name.as_str()) {
                continue;
            }
            log("profile", serde_json::json!({ "ppd_profile": name, "reason": "switch" }));
            st.ppd_profile = Some(name);
            st.apply("profile-change");
        }
        let state = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(POST_SWITCH_RECHECK).await;
            state.lock().unwrap().reconcile("post-switch");
        });
    }
    Err(zbus::Error::Failure("ActiveProfile stream ended".into()))
}

async fn watch_sleep(conn: zbus::Connection, state: Shared) {
    let run = async {
        let logind = Login1ManagerProxy::new(&conn).await?;
        let mut sleeps = logind.receive_prepare_for_sleep().await?;
        while let Some(sig) = sleeps.next().await {
            if sig.args()?.start {
                continue;
            }
            tokio::time::sleep(POST_RESUME_DELAY).await;
            let mut st = state.lock().unwrap();
            log("resume", serde_json::json!({}));
            st.reconcile("resume");
        }
        zbus::Result::Ok(())
    };
    if let Err(e) = run.await {
        log("logind_error", serde_json::json!({ "error": e.to_string() }));
    }
}

fn parse_args() -> PathBuf {
    let mut args = std::env::args().skip(1);
    let mut config = PathBuf::from(SYSTEM_CONFIG_PATH);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => config = args.next().map(PathBuf::from).unwrap_or(config),
            "--version" => {
                println!("thinkpowerd {}", thinkpower_core::VERSION);
                std::process::exit(0);
            }
            other => {
                eprintln!("usage: thinkpowerd [--config PATH]\nunknown argument: {other}");
                std::process::exit(2);
            }
        }
    }
    config
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config_path = parse_args();
    let state: Shared = Arc::new(Mutex::new(State::new(config_path)));
    if let Some(problem) = state.lock().unwrap().caps.blocking_problem() {
        log("unsupported", serde_json::json!({ "problem": problem }));
    }

    let conn = zbus::connection::Builder::system()?
        .name(DBUS_NAME)?
        .serve_at(DBUS_PATH, Daemon { state: state.clone() })?
        .build()
        .await?;

    tokio::spawn(watch_profiles(conn.clone(), state.clone()));
    tokio::spawn(watch_sleep(conn.clone(), state.clone()));

    let mut sample = tokio::time::interval(SAMPLE_EVERY);
    let mut reconcile = tokio::time::interval(RECONCILE_EVERY);
    reconcile.tick().await;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            _ = sample.tick() => state.lock().unwrap().sample(),
            _ = reconcile.tick() => state.lock().unwrap().reconcile("drift"),
            _ = term.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    log("shutdown", serde_json::json!({}));
    Ok(())
}
