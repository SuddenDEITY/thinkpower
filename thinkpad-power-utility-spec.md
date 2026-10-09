# ThinkPad Power Profiles Utility - Product / Engineering Specification

## Goal

Build a reusable Fedora/ThinkPad desktop utility with a GUI that binds explicit CPU power limits in watts to the built-in desktop power profiles.

The intended result is:

- KDE still shows the standard Power Save / Balanced / Performance choices.
- Switching one of those profiles applies user-defined Intel RAPL limits.
- The user can explicitly set PL1, PL2, and Tau for each profile.
- The settings persist across reboot and resume.
- The configuration can be exported, committed to Git, and quickly re-applied on a new compatible laptop.
- The GUI is only a management layer. The actual profile switching must keep working even when the GUI is closed.

The first target system is:

- ThinkPad X1 Nano Gen 3
- Intel Core i7-1370P
- Fedora 44 KDE

The tool should be designed to support other Intel ThinkPads and other Intel laptops with writable RAPL later.

---

## Existing Working Manual Setup

A manual configuration already worked successfully on Fedora 44 KDE.

Fedora used:

- `tuned`
- `tuned-ppd`

rather than `power-profiles-daemon`.

The important integration path was:

```text
KDE power profile selector
        ->
tuned-ppd
        ->
tuned profile
        ->
custom profile script
        ->
Intel RAPL sysfs
```

The profile override root that actually worked was:

```text
/etc/tuned/profiles/
```

not:

```text
/etc/tuned/
```

The working profile directories were:

```text
/etc/tuned/profiles/powersave/
/etc/tuned/profiles/balanced/
/etc/tuned/profiles/throughput-performance/
```

Each profile used a `script.sh` and a `tuned.conf`.

The important `tuned.conf` integration was:

```ini
[script]
script=${i:PROFILE_DIR}/script.sh
```

After moving the customizations under `/etc/tuned/profiles/...`, switching the profile through KDE correctly changed the RAPL power limits.

---

## Known-Good Power Values

These were the final values that worked well on the original X1 Nano Gen 3:

### Power Save

```text
PL1 = 5 W
PL2 = 10 W
Tau = 2 s
```

An earlier 4 W / 8 W experiment was too aggressive. 5 W / 10 W was more usable.

### Balanced

```text
PL1 = 10 W
PL2 = 20 W
Tau = 8 s
```

### Performance

```text
PL1 = 28 W
PL2 = 64 W
Tau = 28 s
```

These values must be treated as a tested reference profile for this laptop, not as universal defaults for every machine.

---

## RAPL Interface

The relevant path was:

```text
/sys/class/powercap/intel-rapl:0/
```

The important files were:

```text
constraint_0_power_limit_uw
constraint_1_power_limit_uw
constraint_0_time_window_us
```

Interpretation:

```text
constraint_0_power_limit_uw = PL1 / sustained package power
constraint_1_power_limit_uw = PL2 / short boost ceiling
constraint_0_time_window_us = Tau / long-term averaging window
```

Conversions:

```text
5 W  = 5000000 uW
10 W = 10000000 uW
28 W = 28000000 uW
64 W = 64000000 uW

2 s  = 2000000 us
8 s  = 8000000 us
28 s = 28000000 us
```

The hardware can quantize requested time windows.

Observed example:

```text
requested: 5000000 us
actual:    4997120 us
```

Therefore the program must always:

1. write the requested value;
2. read it back;
3. display the effective value;
4. distinguish requested from actually accepted hardware values.

---

## Original Stock-Like Values Observed

The original system exposed approximately:

```text
long term power: 64 W
short term power: 64 W
peak power: 140 W
Tau: ~28 s
```

Do not hardcode these assumptions.

The application must discover available zones, constraints, writable paths, and min/max values dynamically.

---

## Core UI

The main screen should show the three system profiles:

```text
Power Save
PL1 [ 5.0 ] W
PL2 [10.0 ] W
Tau [ 2.0 ] s

Balanced
PL1 [10.0 ] W
PL2 [20.0 ] W
Tau [ 8.0 ] s

Performance
PL1 [28.0 ] W
PL2 [64.0 ] W
Tau [28.0 ] s
```

For each profile, show:

- configured PL1;
- configured PL2;
- configured Tau;
- currently effective hardware values;
- whether the profile is currently active;
- whether the last application succeeded;
- whether firmware rounded or rejected any requested value.

The GUI must make it obvious when a configured value differs from the actual effective value.

---

## Built-In Profile Integration

The utility must preserve the normal KDE power profile workflow.

The user should continue to switch profiles through KDE's built-in UI.

The utility should bind:

```text
Power Save  -> custom RAPL policy A
Balanced    -> custom RAPL policy B
Performance -> custom RAPL policy C
```

On the known Fedora setup, use `tuned` + `tuned-ppd`.

The tool should detect:

```text
systemctl status tuned
systemctl status tuned-ppd
```

and detect whether `power-profiles-daemon` is present or active.

Do not blindly enable/disable services.

If conflicting power management stacks are detected, explain the situation and ask before making changes.

---

## Tuned Profile Management

For the Fedora 44 target, the utility should manage or generate profile integration under:

```text
/etc/tuned/profiles/
```

Known profile names:

```text
powersave
balanced
throughput-performance
```

The implementation must research current Fedora/tuned-ppd mapping before coding, because profile names or mapping behavior may change across Fedora releases.

The application must back up existing local profile overrides before changing them.

Do not overwrite unrelated user tuning.

Possible designs:

- carefully manage these exact profile directories; or
- create custom profiles and map them to desktop profiles if current tuned-ppd supports that cleanly.

The key requirement is that KDE's built-in three-profile selector remains functional.

---

## Generated Profile Script Behavior

The manual approach conceptually did:

```bash
echo 5000000  > /sys/class/powercap/intel-rapl:0/constraint_0_power_limit_uw
echo 10000000 > /sys/class/powercap/intel-rapl:0/constraint_1_power_limit_uw
echo 2000000  > /sys/class/powercap/intel-rapl:0/constraint_0_time_window_us
```

Production code must not hardcode the path blindly.

When applying a profile:

1. detect the correct RAPL zone;
2. verify the constraint files exist;
3. validate requested values;
4. write PL1;
5. write PL2;
6. write Tau;
7. read all values back;
8. report the effective values;
9. fail cleanly if firmware rejects the change.

---

## Privilege Model

The GUI should run as the normal user.

Do not run the entire GUI as root.

Preferred architecture:

```text
GUI / CLI
   |
   v
narrow privileged helper
   |
   v
sysfs + /etc/tuned/profiles
```

Good options:

- Polkit-authorized helper;
- D-Bus system service;
- another narrow privileged IPC mechanism.

The privileged API should expose only explicit operations such as:

```text
read_capabilities()
apply_limits(pl1, pl2, tau)
install_profile_integration()
restore_original_configuration()
```

Do not provide arbitrary root shell execution.

Do not install broad passwordless sudo rules.

---

## Hardware Discovery

On startup detect:

- CPU vendor;
- CPU model;
- Intel RAPL presence;
- RAPL zones;
- constraint names;
- current values;
- exposed min/max values;
- write capability;
- ACPI platform profile availability.

Important paths may include:

```text
/sys/class/powercap/intel-rapl:0/
/sys/firmware/acpi/platform_profile
```

Example status:

```text
Intel RAPL detected
Package zone: intel-rapl:0
PL1 writable: yes
PL2 writable: yes
Tau writable: yes
```

Unsupported systems should fail clearly rather than partially configuring the machine.

---

## Validation

Use sysfs-exposed min/max limits whenever available.

The GUI should reject clearly nonsensical values.

Basic normal validation:

```text
PL1 > 0
PL2 > 0
Tau > 0
PL1 <= PL2
```

If hardware reports a lower maximum than an imported config requests, do not silently clamp without telling the user.

Example:

```text
Imported PL2: 64 W
Hardware maximum: 45 W

[Use 45 W]
[Edit]
[Cancel]
```

---

## ACPI Platform Profile

The machine exposed:

```text
/sys/firmware/acpi/platform_profile
```

with choices:

```text
low-power
balanced
performance
```

A previous observation was that KDE/tuned could be in Performance while this file still showed `balanced`.

Therefore the utility must model these as separate control layers:

1. KDE/tuned profile;
2. Intel RAPL limits;
3. ACPI/Lenovo platform profile.

Do not assume they are equivalent.

The UI may optionally allow a mapping such as:

```text
Power Save  -> low-power
Balanced    -> balanced
Performance -> balanced
```

or let the user choose `performance`.

For the original laptop, leaving ACPI platform profile at `balanced` while controlling package power through RAPL was considered acceptable.

---

## Thermal / Lenovo DPTF Caveat

KDE previously displayed a warning similar to:

```text
Performance may be lowered because the computer has detected it may be sitting on your lap.
```

The machine exposed Lenovo/DPTF/proc_thermal behavior.

No safe definitive lap-detection bypass was established.

The utility must not:

- disable thermal protection;
- bypass DPTF safety logic;
- defeat firmware thermal limits.

Instead distinguish:

- requested RAPL limit;
- effective RAPL value;
- thermal throttling;
- platform/firmware throttling.

The program should make it clear that a requested 28 W ceiling does not guarantee the CPU will actually consume 28 W.

---

## Wi-Fi Power Saving

The previous power-save script originally interacted with Wi-Fi power saving.

That behavior was intentionally removed because of concerns about VPN/network stability.

Therefore:

```text
Default: never modify Wi-Fi power saving.
```

CPU package power policy must remain separate from Wi-Fi tuning.

If Wi-Fi power saving is ever added, make it an explicit independent option and keep it off by default.

---

## Suspend / Resume

RAPL limits may reset after suspend/resume or firmware events.

The tool should verify limits after resume.

Desired behavior:

```text
resume
  ->
read active desktop profile
  ->
read current RAPL values
  ->
compare to configured values
  ->
reapply only if necessary
```

Implementation options:

- systemd sleep hooks;
- logind D-Bus `PrepareForSleep`;
- tuned re-application;
- a lightweight system service.

Avoid unnecessary repeated writes.

---

## Boot Persistence

The system must work after reboot without manually opening the GUI.

Configuration should persist in a versioned config.

Example:

```toml
[profiles.power_save]
pl1_w = 5.0
pl2_w = 10.0
tau_s = 2.0

[profiles.balanced]
pl1_w = 10.0
pl2_w = 20.0
tau_s = 8.0

[profiles.performance]
pl1_w = 28.0
pl2_w = 64.0
tau_s = 28.0
```

The GUI edits this policy, but the actual profile behavior must continue headlessly.

---

## Import / Export

A main purpose of the tool is fast reuse on a new laptop.

Support:

```text
Export configuration
Import configuration
```

A portable file could look like:

```yaml
version: 1

profiles:
  power_save:
    pl1_w: 5
    pl2_w: 10
    tau_s: 2

  balanced:
    pl1_w: 10
    pl2_w: 20
    tau_s: 8

  performance:
    pl1_w: 28
    pl2_w: 64
    tau_s: 28
```

When importing on new hardware:

1. discover capabilities;
2. compare requested values to supported values;
3. show compatibility report;
4. let the user adjust;
5. only then apply.

This exported config should be suitable for committing to Git.

---

## First-Run Wizard

The first-run setup should make configuring a fresh ThinkPad take only a few minutes.

Flow:

1. Detect Fedora version.
2. Detect KDE / power profile stack.
3. Detect `tuned`.
4. Detect `tuned-ppd`.
5. Detect conflicting `power-profiles-daemon`.
6. Detect Intel RAPL.
7. Verify writable controls through the privileged helper.
8. Read current values.
9. Offer:
   - Import Config
   - Manual Setup
   - Known X1 Nano Example
10. Install integration.
11. Apply Balanced.
12. Read values back.
13. Show success.

---

## Live Monitoring

Useful optional live data:

- active KDE/tuned profile;
- configured PL1/PL2/Tau;
- effective PL1/PL2/Tau;
- current package power;
- CPU temperature;
- optionally frequencies.

Previously `powertop` was installed and `turbostat` was suggested for verifying actual package power.

The application should prefer direct stable kernel/sysfs interfaces where possible, and use external utilities only when necessary.

Example dashboard:

```text
Active profile: Balanced

Configured
PL1: 10 W
PL2: 20 W
Tau: 8 s

Effective
PL1: 10.0 W
PL2: 20.0 W
Tau: 7.99 s

Package power now: 7.4 W
Temperature: 62 C
```

---

## CLI

The project should include a CLI in addition to the GUI.

Example:

```text
thinkpower status
thinkpower capabilities
thinkpower profiles
thinkpower set power-save --pl1 5 --pl2 10 --tau 2
thinkpower set balanced --pl1 10 --pl2 20 --tau 8
thinkpower set performance --pl1 28 --pl2 64 --tau 28
thinkpower apply balanced
thinkpower verify
thinkpower install
thinkpower restore
thinkpower export config.yaml
thinkpower import config.yaml
```

GUI and CLI must share the same core library.

---

## Restore / Uninstall

This is mandatory.

Before modifying tuned configuration:

- create backups;
- record what the utility owns.

Provide a one-click:

```text
Restore Fedora Defaults
```

It should:

- remove generated integration;
- restore previous local files;
- restart/reload required services;
- verify standard profile switching still works.

Uninstalling the application must not leave a broken power-management configuration.

---

## Logging and Diagnostics

Use structured logs.

Example:

```json
{
  "event": "rapl_apply",
  "profile": "balanced",
  "requested": {
    "pl1_w": 10.0,
    "pl2_w": 20.0,
    "tau_s": 8.0
  },
  "effective": {
    "pl1_w": 10.0,
    "pl2_w": 20.0,
    "tau_s": 7.995
  },
  "success": true
}
```

Provide a diagnostics view/command with:

- CPU model;
- kernel version;
- Fedora version;
- tuned version;
- tuned-ppd version;
- current tuned profile;
- desktop power profile;
- ACPI platform profile;
- RAPL tree;
- current RAPL values;
- whether controls are writable.

Do not log unrelated personal data.

---

## Error UX

Examples:

```text
Performance profile could not be applied exactly.

Requested PL2: 64 W
Effective PL2: 45 W

The firmware may be enforcing a lower maximum.
```

```text
Intel RAPL is present but read-only.
This laptop cannot currently use custom package power limits.
```

```text
Both tuned-ppd and power-profiles-daemon appear active.
This can make desktop power profile behavior unpredictable.
```

Errors must be actionable and explain what the program changed or did not change.

---

## Architecture Recommendation

A clean project layout could be:

```text
thinkpower/
  core/
    config
    validation
    profiles
    capabilities
    monitoring

  backends/
    rapl
    tuned
    tuned_ppd
    acpi_platform_profile

  privileged/
    helper
    polkit

  cli/

  ui/

  packaging/
    fedora
```

A compiled language is preferable.

Rust is a strong candidate because it fits:

- sysfs;
- D-Bus;
- Polkit;
- CLI/core reuse;
- robust error handling;
- desktop packaging.

Possible GUI stacks:

- Tauri;
- GTK;
- another mature Linux-first desktop toolkit.

Do not choose technology only for UI convenience. Privilege separation and reliability matter more.

---

## Packaging / Git Goal

The repository should make setup on a fresh laptop straightforward.

Ideal development flow:

```text
git clone ...
cd thinkpower
./install.sh
```

Longer-term:

```text
sudo dnf install thinkpower
```

Release targets:

- Fedora RPM;
- optionally AppImage;
- source build instructions.

The repository README should explain:

- supported CPUs/OS;
- PL1 / PL2 / Tau;
- KDE/tuned integration;
- security model;
- restore procedure;
- importing/exporting profiles.

---

## Safety Rules

Never:

- disable thermal protection;
- disable DPTF protections;
- bypass firmware thermal shutdown;
- assume all ThinkPads use the same RAPL indexes;
- silently exceed hardware-reported max;
- run the full GUI as root;
- install broad passwordless sudo;
- overwrite tuned config without backup;
- modify Wi-Fi power save by default.

Always:

- discover capabilities;
- validate requested values;
- read values back;
- show effective values;
- preserve rollback;
- keep firmware safety mechanisms intact.

---

## Testing

### Unit tests

- watts <-> microwatts conversion;
- seconds <-> microseconds conversion;
- rounded Tau values;
- config parsing;
- profile validation;
- capability parsing.

### Mock sysfs tests

Simulate:

- writable RAPL;
- read-only RAPL;
- missing PL2;
- rounded Tau;
- rejected values;
- multiple RAPL zones.

### Fedora integration tests

Verify:

```text
KDE Power Save  -> configured PL1/PL2/Tau
KDE Balanced    -> configured PL1/PL2/Tau
KDE Performance -> configured PL1/PL2/Tau
```

### Lifecycle tests

- reboot;
- suspend/resume;
- tuned restart;
- GUI closed;
- restore defaults.

---

## Acceptance Criteria

On a compatible fresh Fedora laptop:

1. Install ThinkPower.
2. Open GUI.
3. Hardware and power stack are detected.
4. User enters explicit values for Power Save / Balanced / Performance.
5. Click Apply.
6. Integration is installed safely.
7. KDE's built-in profile selector remains the normal way to switch profiles.
8. Switching KDE Power Save applies the configured Power Save values.
9. Switching Balanced applies the configured Balanced values.
10. Switching Performance applies the configured Performance values.
11. The GUI shows requested and effective RAPL values.
12. Settings survive reboot.
13. Settings are rechecked/reapplied after resume if necessary.
14. Config can be exported to Git and imported on another machine.
15. Restore Defaults fully removes the utility's changes.

---

## Known-Good Test Fixture

Use this as a regression fixture only:

```yaml
machine:
  model: ThinkPad X1 Nano Gen 3
  cpu: Intel Core i7-1370P
  os: Fedora 44 KDE

profiles:
  power_save:
    pl1_w: 5
    pl2_w: 10
    tau_s: 2

  balanced:
    pl1_w: 10
    pl2_w: 20
    tau_s: 8

  performance:
    pl1_w: 28
    pl2_w: 64
    tau_s: 28
```

Known target path:

```text
/sys/class/powercap/intel-rapl:0/
```

Known files:

```text
constraint_0_power_limit_uw
constraint_1_power_limit_uw
constraint_0_time_window_us
```

Known tuned profile root:

```text
/etc/tuned/profiles/
```

Known profile directories:

```text
powersave
balanced
throughput-performance
```

Known integration:

```text
KDE
  ->
tuned-ppd
  ->
tuned
  ->
RAPL
```

Do not bury these as unexplained production constants.

---

## Implementation Phases

### Phase 0 - Research

Before coding, verify current Fedora behavior:

- tuned/tuned-ppd profile mapping;
- package/service names;
- current `/etc/tuned/profiles` semantics;
- KDE profile switching;
- Intel RAPL schema;
- Polkit/D-Bus implementation options.

Produce findings first.

### Phase 1 - Core + CLI

Implement:

```text
status
capabilities
read
apply
verify
```

### Phase 2 - Tuned Integration

Make KDE switching drive the configured values.

### Phase 3 - Privileged Helper

Move all privileged writes behind a narrow secure API.

### Phase 4 - Persistence

Reboot + suspend/resume.

### Phase 5 - GUI

Editable profile cards, monitoring, validation, restore.

### Phase 6 - Import/Export

Portable Git-friendly config with hardware compatibility checks.

### Phase 7 - Packaging

RPM and release workflow.

---

## Final Instruction to the Coding Agent

Do not start with the GUI.

First prove the entire control path:

```text
KDE profile switch
    ->
tuned-ppd
    ->
correct tuned profile
    ->
privileged RAPL write
    ->
hardware readback
```

The critical invariants are:

1. KDE's built-in power profile selector remains the primary switching mechanism.
2. Each built-in profile maps to explicit user-configured PL1 / PL2 / Tau values.
3. Every requested hardware write is verified by reading the effective value back.
4. Firmware and thermal protections remain untouched.
5. All system modifications are reversible.
6. The configuration is portable, versioned, and suitable for Git.
7. The GUI is not required to remain running for profile switching to work.

Research the current Fedora implementation first, then build the smallest end-to-end reliable version before expanding the product.
