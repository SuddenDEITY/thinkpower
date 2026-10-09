# ThinkPower

Set your own CPU power limits (PL1 / PL2 / Tau) for each of the desktop's
built-in power profiles (**Power Save**, **Balanced**, **Performance**) on
Intel laptops running Fedora.

You keep switching profiles the usual way, from the KDE (or GNOME) power menu.
ThinkPower applies the limits you set for that profile, reads them back from
the hardware, and applies them again after resume. The GUI is only for
editing and monitoring. Nothing has to stay open.

```
git clone <this repo> && cd thinkpower
./install.sh
```

## What PL1, PL2 and Tau mean

| | Sysfs file | Meaning |
|---|---|---|
| **PL1** | `constraint_0_power_limit_uw` (`long_term`) | Sustained package power. |
| **PL2** | `constraint_1_power_limit_uw` (`short_term`) | Short boost ceiling. |
| **Tau** | `constraint_0_time_window_us` | Time window that PL1 is averaged over. |

The CPU can run up to PL2 for as long as its average power over the last Tau
seconds stays under PL1. Hardware rounds some values (Tau especially, so 8 s
may read back as 7.995 s). ThinkPower always shows the value the hardware
actually holds next to the value you asked for.

These limits are ceilings, not targets. Thermal and firmware protection (DPTF,
lap detection) can still keep the CPU below them. ThinkPower never touches
those protections.

## Supported systems

- Intel CPUs with writable RAPL (`/sys/class/powercap/intel-rapl:*`). Run
  `thinkpower capabilities` to check yours.
- Fedora with `tuned` + `tuned-ppd` (the default on Fedora 41 and later).
  `power-profiles-daemon` works too, since it exposes the same D-Bus API.
- Tested on a ThinkPad X1 Nano Gen 3 (i7-1370P) running Fedora 44 KDE.

## How it works

```
KDE power profile switch
  → tuned-ppd (org.freedesktop.UPower.PowerProfiles: ActiveProfile)
      → thinkpowerd (system service)
          → writes PL1/PL2/Tau to intel-rapl:<package>, reads back, logs
```

- **`thinkpowerd`** (root, sandboxed systemd service):
  - follows `ActiveProfile` and applies the configured limits for the active profile;
  - re-checks after resume (logind `PrepareForSleep`) and every 10 s, and writes again only if the hardware drifted;
  - samples package, platform and battery power once a second, keeps 30 s and 1 min averages, and estimates battery life;
  - finds RAPL zones and constraints by name, never by fixed index.
- **`thinkpower`** (your user): runs the GUI with no arguments, otherwise works as a CLI. It talks to the daemon over D-Bus.
- **Config:** `/etc/thinkpower/config.toml`, versioned TOML. Example: [`configs/x1-nano-gen3.toml`](configs/x1-nano-gen3.toml).

ThinkPower does **not** modify tuned profiles or `ppd.conf`. Fedora's stock
tuned tuning (EPP, governor, platform_profile, …) keeps working. ThinkPower only
adds the RAPL limits on top. If you want, you can also override
`platform_profile` for each profile (for example, keep `balanced` in Performance).

### Two RAPL interfaces

Many Intel laptops expose both `intel-rapl` (MSR) and `intel-rapl-mmio`, which
firmware/DPTF controls. The CPU enforces the **lower** of the two. ThinkPower
writes only the MSR limits and shows the MMIO limits so you can see when
firmware is the one holding power down.

## Security model

- The GUI and CLI never run as root.
- Changing the config goes through one D-Bus method (`SetConfig`). The daemon
  validates every config and checks polkit before accepting it. The polkit
  action is `io.github.thinkpower.configure`, and it asks for an admin password
  (kept for a few minutes).
- Reading status needs no authorization. RAPL energy counters stay root-only.
  The daemon publishes only 1-second averages.
- Restore runs through `pkexec` on a single script
  (`io.github.thinkpower.restore`).
- There are no sudo rules and no generic "run as root" API.

## Using it

GUI: open **ThinkPower** from the app menu. Edit the values and click
**Apply**. You'll be asked for your password. The active profile card shows the
values currently in the hardware: green means exact, amber means rounded, red
means different from what you asked for.

Tray: `thinkpower tray` starts automatically when you log in. On battery, the
icon shows how long the battery will last at the current pace, for example
`2.5h` or `30m`. On AC it shows `AC`. The badge colour shows the profile: green
for Power Save, blue for Balanced, orange for Performance. The tooltip shows the
estimate in full ("about 2 h 30 min left") or the time to full charge, plus the
laptop's 1-minute average draw, CPU package power, the current limits and the
temperature. Click the icon to open the window and click again to close it. A
window with unsaved edits is never closed from the tray. Right-click the icon to
switch profiles. If you use "Hide tray icon", the tray comes back at your next
login. To turn it off for good, disable it in System Settings → Autostart.

Battery life is estimated as the remaining energy (Wh) divided by the average
draw over the last minute, so it reacts to what you're doing right now.

CLI:

```
thinkpower status                       # active profile, configured vs effective, live power
thinkpower capabilities                 # what this hardware supports (no daemon needed)
thinkpower profiles
thinkpower set power-save --pl1 6 --pl2 12 --tau 3
thinkpower set performance --platform-profile balanced
thinkpower apply                        # re-apply the active profile now
thinkpower verify                       # exit 1 if hardware doesn't match
thinkpower export my-laptop.toml
thinkpower import my-laptop.toml [--clamp] [--yes]
thinkpower check my-laptop.toml         # compatibility report only
thinkpower diagnostics
thinkpower tray                         # system tray icon
thinkpower restore
```

Logs are structured JSON in the journal: `journalctl -u thinkpowerd`.

## Moving to another laptop

1. On the old machine, export your config to `configs/<model>.toml`. Use the
   GUI's **Export…** or `thinkpower export`. Commit and push it.
2. On the new machine, run `git clone … && ./install.sh`.

`install.sh` picks the config whose `machine.model` matches this laptop's DMI
model and shows a compatibility report. If a value is above what the new
hardware reports, the daemon does not apply it and nothing is clamped silently.
The GUI shows the problem with a one-click **Use N W** fix. You can also run
`thinkpower import --clamp`.

`install.sh` also finds old hand-written tuned overrides that write RAPL
(for example `/etc/tuned/profiles/powersave/script.sh`). It lists them, asks
before doing anything, and moves them to `/var/lib/thinkpower/backup/<timestamp>/`.

## Restore / uninstall

Run `./uninstall.sh` or click **Restore defaults…** in the GUI.

- It removes the daemon, the config and every installed file, then restarts
  tuned so the stock profiles are active again.
- It saves your config to `/var/lib/thinkpower/last-config.toml`.
- `./uninstall.sh --restore-backup` also puts back the tuned overrides that
  were moved at install.
- RAPL limits keep their current values until the next reboot, when firmware
  defaults return.

## Building / development

```
cargo build --release
cargo test               # core logic runs against a mock sysfs
```

Layout:

| Path | Contents |
|---|---|
| `crates/core` | config, validation, RAPL discovery/apply/readback, monitoring (no root, no D-Bus) |
| `crates/daemon` | `thinkpowerd` |
| `crates/app` | `thinkpower` GUI (egui), tray (StatusNotifierItem) and CLI |
| `packaging/` | systemd unit, D-Bus policy, polkit policy, .desktop files (app and tray autostart) |
| `configs/` | per-machine configs |
