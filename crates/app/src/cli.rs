//! Command-line interface. Shares the core library and daemon client with the GUI.

use crate::client::Client;
use clap::{Parser, Subcommand};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use thinkpower_core::caps::{self, Capabilities};
use thinkpower_core::config::{Config, ProfileId, SYSTEM_CONFIG_PATH};
use thinkpower_core::rapl::{FieldReport, HwLimits, Outcome};
use thinkpower_core::status::Status;
use thinkpower_core::sysfs::RealSysfs;
use thinkpower_core::validate::{self, Issue, Severity};

#[derive(Parser)]
#[command(name = "thinkpower", version, about = "Bind Intel RAPL power limits to the desktop power profiles")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Open the GUI (default).
    Gui,
    /// Run the system tray icon.
    Tray,
    /// Active profile, configured vs effective limits, live power.
    Status,
    /// Detected hardware and controls (works without the daemon).
    Capabilities,
    /// Show the configured profiles.
    Profiles,
    /// Change one profile's limits.
    Set {
        profile: String,
        #[arg(long)]
        pl1: Option<f64>,
        #[arg(long)]
        pl2: Option<f64>,
        #[arg(long)]
        tau: Option<f64>,
        /// Override the ACPI platform_profile for this profile (e.g. balanced).
        #[arg(long, conflicts_with = "no_platform_profile")]
        platform_profile: Option<String>,
        /// Leave platform_profile to tuned.
        #[arg(long)]
        no_platform_profile: bool,
    },
    /// Re-apply the active profile's configured limits now.
    Apply,
    /// Check that hardware matches the configured limits; exit 1 if not.
    Verify,
    /// Write the current configuration to a file.
    Export { file: PathBuf },
    /// Load a configuration file, check it against this hardware and install it.
    Import {
        file: PathBuf,
        /// Accept hardware maximums where the file asks for more.
        #[arg(long)]
        clamp: bool,
        /// Don't ask for confirmation.
        #[arg(long, short)]
        yes: bool,
    },
    /// Validate a configuration file against this hardware (no changes).
    Check { file: PathBuf },
    /// Print diagnostics for bug reports.
    Diagnostics,
    /// Remove ThinkPower and restore Fedora's power profile defaults.
    Restore,
}

pub fn run(cmd: Command) -> ExitCode {
    let res = match cmd {
        Command::Gui | Command::Tray => unreachable!(),
        Command::Status => status(),
        Command::Capabilities => {
            let c = caps::discover(&RealSysfs);
            print_caps(&c);
            Ok(if c.blocking_problem().is_some() { ExitCode::FAILURE } else { ExitCode::SUCCESS })
        }
        Command::Profiles => profiles(),
        Command::Set { profile, pl1, pl2, tau, platform_profile, no_platform_profile } => {
            set(&profile, pl1, pl2, tau, platform_profile, no_platform_profile)
        }
        Command::Apply => connect().and_then(|c| c.reapply()).map(|s| {
            print_status(&s);
            verdict(&s)
        }),
        Command::Verify => connect().and_then(|c| c.status()).map(|s| {
            print_status(&s);
            verdict(&s)
        }),
        Command::Export { file } => export(&file),
        Command::Import { file, clamp, yes } => import(&file, clamp, yes),
        Command::Check { file } => check(&file),
        Command::Diagnostics => diagnostics(),
        Command::Restore => restore(),
    };
    match res {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn connect() -> Result<Client, String> {
    Client::connect()
}

pub fn fmt_w(v: Option<f64>) -> String {
    v.map(|v| format!("{v:.1} W")).unwrap_or_else(|| "—".into())
}

pub fn fmt_s(v: Option<f64>) -> String {
    v.map(|v| if v < 10.0 { format!("{v:.3} s") } else { format!("{v:.2} s") }).unwrap_or_else(|| "—".into())
}

pub fn outcome_text(o: &Outcome) -> String {
    match o {
        Outcome::Exact => "exact".into(),
        Outcome::Rounded => "rounded by hardware".into(),
        Outcome::Mismatch => "DIFFERS (firmware may enforce another limit)".into(),
        Outcome::Rejected(e) => format!("REJECTED: {e}"),
        Outcome::Unsupported => "not supported".into(),
    }
}

fn hw_line(l: Option<HwLimits>) -> String {
    match l {
        Some(l) => format!("PL1 {:>7}  PL2 {:>7}  Tau {}", fmt_w(Some(l.pl1_w)), fmt_w(l.pl2_w), fmt_s(Some(l.tau_s))),
        None => "—".into(),
    }
}

fn field_line(name: &str, f: &FieldReport, unit_s: bool) -> String {
    let (req, eff) = if unit_s { (fmt_s(Some(f.requested)), fmt_s(f.effective)) } else { (fmt_w(Some(f.requested)), fmt_w(f.effective)) };
    format!("  {name:<4} requested {req:>9}  effective {eff:>9}  {}", outcome_text(&f.outcome))
}

fn print_status(s: &Status) {
    let active = s.active.map(|p| p.label().to_string()).or(s.ppd_profile.clone()).unwrap_or_else(|| "unknown".into());
    println!("Active profile:    {active}");
    if let Some(e) = &s.config_error {
        println!("Config problem:    {e}");
    }
    if let (Some(cfg), Some(p)) = (&s.config, s.active) {
        let c = cfg.profiles.get(p);
        println!("Configured:        PL1 {:>7}  PL2 {:>7}  Tau {}", fmt_w(Some(c.pl1_w)), fmt_w(Some(c.pl2_w)), fmt_s(Some(c.tau_s)));
    }
    println!("Effective (MSR):   {}", hw_line(s.effective));
    if s.firmware_mmio.is_some() {
        println!("Firmware (MMIO):   {}", hw_line(s.firmware_mmio));
    }
    if let Some(pp) = &s.platform_profile {
        println!("Platform profile:  {pp}");
    }
    if let Some(r) = &s.last_apply {
        println!("Last apply ({}, {}): {}", r.profile, r.reason, if r.success { "ok" } else { "PROBLEM" });
        println!("{}", field_line("PL1", &r.pl1, false));
        println!("{}", field_line("PL2", &r.pl2, false));
        println!("{}", field_line("Tau", &r.tau, true));
        if let Some(pp) = &r.platform_profile {
            println!("  platform_profile requested {} effective {}", pp.requested, pp.effective.as_deref().unwrap_or("—"));
        }
    }
    let p = &s.power;
    println!();
    println!("Package power:     {:>7} now  {:>7} avg {:.0}s", fmt_w(p.package_w), fmt_w(p.package_avg_w), p.window_s);
    if p.psys_w.is_some() {
        println!("Platform (psys):   {:>7} now  {:>7} avg {:.0}s", fmt_w(p.psys_w), fmt_w(p.psys_avg_w), p.window_s);
    }
    if p.battery_w.is_some() {
        println!("Battery draw:      {:>7} now  {:>7} avg {:.0}s", fmt_w(p.battery_w), fmt_w(p.battery_avg_w), p.window_s);
    }
    if let Some(t) = p.cpu_temp_c {
        println!("CPU temperature:   {t:.0} °C");
    }
    for w in &s.warnings {
        println!("warning: {w}");
    }
}

fn verdict(s: &Status) -> ExitCode {
    let ok = s.config_error.is_none() && s.last_apply.as_ref().is_some_and(|r| r.success);
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

fn status() -> Result<ExitCode, String> {
    let s = connect()?.status()?;
    print_status(&s);
    Ok(ExitCode::SUCCESS)
}

pub fn print_caps(c: &Capabilities) {
    println!("Machine:           {}", c.machine_model);
    println!("CPU:               {} ({})", c.cpu_model, c.cpu_vendor);
    println!("OS / kernel:       {} / {}", c.os, c.kernel);
    match &c.rapl {
        Some(z) => {
            println!("Intel RAPL:        {} ({}) at {}", z.id, z.name, z.path);
            let yn = |b: bool| if b { "yes" } else { "no" };
            println!("  PL1 writable:    {}   max {}", yn(z.pl1_writable), fmt_w(z.pl1_max_w));
            println!(
                "  PL2 writable:    {}   max {}",
                if z.pl2_present { yn(z.pl2_writable) } else { "not present" },
                z.pl2_max_w.map(|v| format!("{v:.1} W")).unwrap_or_else(|| "not reported".into())
            );
            println!("  Tau writable:    {}", yn(z.tau_writable));
            println!("  Current:         {}", hw_line(z.current));
        }
        None => println!("Intel RAPL:        not available ({})", c.rapl_error.as_deref().unwrap_or("unknown")),
    }
    if let Some(m) = &c.mmio {
        println!("Firmware MMIO:     {} {}", m.id, hw_line(m.current));
    }
    match &c.platform_profile {
        Some(p) => println!("Platform profile:  {} (choices: {})", p.current, p.choices.join(", ")),
        None => println!("Platform profile:  not available"),
    }
    if let Some(problem) = c.blocking_problem() {
        println!("\nUNSUPPORTED: {problem}");
    }
}

fn print_profiles(cfg: &Config) {
    for id in ProfileId::ALL {
        let p = cfg.profiles.get(id);
        print!("{:<12} PL1 {:>7}  PL2 {:>7}  Tau {:>6.1} s", id.label(), fmt_w(Some(p.pl1_w)), fmt_w(Some(p.pl2_w)), p.tau_s);
        if let Some(pp) = &p.platform_profile {
            print!("  platform_profile={pp}");
        }
        println!();
    }
}

/// Current config: from the daemon if it runs, else the system file.
fn current_config() -> Result<Config, String> {
    if let Ok(c) = connect() {
        if let Ok(s) = c.status() {
            if let Some(cfg) = s.config {
                return Ok(cfg);
            }
        }
    }
    Config::load(Path::new(SYSTEM_CONFIG_PATH)).map_err(|e| e.to_string())
}

fn profiles() -> Result<ExitCode, String> {
    print_profiles(&current_config()?);
    Ok(ExitCode::SUCCESS)
}

fn print_issues(issues: &[Issue]) {
    for i in issues {
        let tag = match i.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        let hint = i.suggested.map(|v| format!(" (suggested: {v})")).unwrap_or_default();
        println!("  {tag}: {}: {}{hint}", i.profile, i.message);
    }
}

fn submit(cfg: &Config) -> Result<ExitCode, String> {
    let s = connect()?.set_config(cfg)?;
    print_status(&s);
    Ok(verdict(&s))
}

fn set(
    profile: &str,
    pl1: Option<f64>,
    pl2: Option<f64>,
    tau: Option<f64>,
    platform_profile: Option<String>,
    no_platform_profile: bool,
) -> Result<ExitCode, String> {
    let id = ProfileId::parse(profile).ok_or_else(|| format!("unknown profile {profile:?} (power-save, balanced, performance)"))?;
    let mut cfg = current_config()?;
    let p = cfg.profiles.get_mut(id);
    if let Some(v) = pl1 {
        p.pl1_w = v;
    }
    if let Some(v) = pl2 {
        p.pl2_w = v;
    }
    if let Some(v) = tau {
        p.tau_s = v;
    }
    if platform_profile.is_some() {
        p.platform_profile = platform_profile;
    }
    if no_platform_profile {
        p.platform_profile = None;
    }
    let issues = validate::validate(&cfg, Some(&caps::discover(&RealSysfs)));
    print_issues(&issues);
    if validate::has_errors(&issues) {
        return Ok(ExitCode::FAILURE);
    }
    submit(&cfg)
}

fn export(file: &Path) -> Result<ExitCode, String> {
    let mut cfg = current_config()?;
    let c = caps::discover(&RealSysfs);
    let m = cfg.machine.get_or_insert_with(Default::default);
    m.model.get_or_insert(c.machine_model);
    m.cpu.get_or_insert(c.cpu_model);
    std::fs::write(file, cfg.to_toml()).map_err(|e| format!("{}: {e}", file.display()))?;
    println!("Exported to {}", file.display());
    Ok(ExitCode::SUCCESS)
}

/// Load, check against hardware, optionally clamp. Prints a compatibility report.
fn load_checked(file: &Path, clamp: bool) -> Result<(Config, Vec<Issue>), String> {
    let mut cfg = Config::load(file).map_err(|e| e.to_string())?;
    let c = caps::discover(&RealSysfs);
    if let Some(problem) = c.blocking_problem() {
        return Err(problem);
    }
    if let Some(m) = &cfg.machine {
        if let Some(model) = &m.model {
            if *model != c.machine_model {
                println!("Note: this config was made for {model}; this machine is {}.", c.machine_model);
            }
        }
    }
    let mut issues = validate::validate(&cfg, Some(&c));
    if clamp {
        for i in issues.iter().filter(|i| i.severity == Severity::Error) {
            if validate::apply_suggestion(&mut cfg, i) {
                println!("  clamped {} {:?} to {}", i.profile, i.field, i.suggested.unwrap_or_default());
            }
        }
        issues = validate::validate(&cfg, Some(&c));
    }
    println!("Profiles in {}:", file.display());
    print_profiles(&cfg);
    if issues.is_empty() {
        println!("Compatibility: OK");
    } else {
        println!("Compatibility:");
        print_issues(&issues);
    }
    Ok((cfg, issues))
}

fn check(file: &Path) -> Result<ExitCode, String> {
    let (_, issues) = load_checked(file, false)?;
    Ok(if validate::has_errors(&issues) { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

fn confirm(question: &str) -> bool {
    print!("{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    matches!(line.trim(), "y" | "Y" | "yes")
}

fn import(file: &Path, clamp: bool, yes: bool) -> Result<ExitCode, String> {
    let (cfg, issues) = load_checked(file, clamp)?;
    if validate::has_errors(&issues) {
        println!("\nNot imported. Fix the values, or use --clamp to accept the hardware maximums.");
        return Ok(ExitCode::FAILURE);
    }
    if !yes && !confirm("Install this configuration?") {
        return Ok(ExitCode::FAILURE);
    }
    submit(&cfg)
}

fn command_output(cmd: &str, args: &[&str]) -> String {
    std::process::Command::new(cmd)
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(if o.status.success() { &o.stdout } else { &o.stderr }).trim().to_string())
        .unwrap_or_else(|e| format!("({cmd}: {e})"))
}

fn diagnostics() -> Result<ExitCode, String> {
    print_caps(&caps::discover(&RealSysfs));
    println!();
    println!("Packages:          {}", command_output("rpm", &["-q", "tuned", "tuned-ppd", "power-profiles-daemon"]).replace('\n', ", "));
    for svc in ["tuned", "tuned-ppd", "power-profiles-daemon", "thinkpowerd"] {
        println!("{:<19}{}", format!("{svc}:"), command_output("systemctl", &["is-active", svc]));
    }
    println!("tuned profile:     {}", command_output("tuned-adm", &["active"]));
    println!();
    match connect().and_then(|c| c.status()) {
        Ok(s) => print_status(&s),
        Err(e) => println!("Daemon: {e}"),
    }
    Ok(ExitCode::SUCCESS)
}

fn restore() -> Result<ExitCode, String> {
    if !confirm("Remove ThinkPower and restore Fedora's power profile defaults?") {
        return Ok(ExitCode::FAILURE);
    }
    let st = std::process::Command::new("pkexec")
        .args([thinkpower_core::RESTORE_HELPER, "--yes"])
        .status()
        .map_err(|e| format!("pkexec: {e}"))?;
    Ok(if st.success() { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}
