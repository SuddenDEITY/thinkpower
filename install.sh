#!/usr/bin/env bash
# ThinkPower installer. Run as your normal user from the repository checkout:
#
#   ./install.sh                  # pick configs/<this machine>.toml, or keep /etc/thinkpower/config.toml
#   ./install.sh --config FILE    # install a specific config
#   ./install.sh --yes            # don't ask
#
# Builds with cargo as you, then uses sudo once for the system install.
set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")"
REPO=$PWD
PREFIX=/usr/local
LIBEXEC=$PREFIX/libexec/thinkpower
STATE=/var/lib/thinkpower
CONFIG=/etc/thinkpower/config.toml

YES=0
CONFIG_SRC=""
while [[ $# -gt 0 ]]; do
    case $1 in
        --yes|-y) YES=1 ;;
        --config) CONFIG_SRC=$(readlink -f "$2"); shift ;;
        -h|--help) sed -n '2,9p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

say()  { printf '\033[1;34m==>\033[0m \033[1m%s\033[0m\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
ask()  { [[ $YES == 1 ]] && return 0; local a; read -r -p "$1 [y/N] " a; [[ $a == [yY]* ]]; }

[[ $EUID -ne 0 ]] || die "run as your normal user; sudo is used only for the install step"

# --- 1. Desktop power profile stack -----------------------------------------
say "Checking the power profile stack"
tuned_ppd=$(systemctl is-active tuned-ppd 2>/dev/null || true)
ppd=$(systemctl is-active power-profiles-daemon 2>/dev/null || true)
echo "    tuned-ppd: $tuned_ppd    power-profiles-daemon: $ppd"
if [[ $tuned_ppd == active && $ppd == active ]]; then
    die "Both tuned-ppd and power-profiles-daemon are active. This makes desktop power profiles unpredictable.
       Disable one of them first (Fedora uses tuned-ppd). Nothing was changed."
fi
[[ $tuned_ppd == active || $ppd == active ]] || die "No power profile service is running (expected tuned-ppd). Nothing was changed."

# --- 2. Build ------------------------------------------------------------------
CARGO=$(command -v cargo || true)
[[ -n $CARGO ]] || CARGO=$HOME/.cargo/bin/cargo
[[ -x $CARGO ]] || die "cargo not found. Install Rust (https://rustup.rs or: sudo dnf install cargo) and re-run."
say "Building (cargo build --release)"
"$CARGO" build --release --locked
BIN=$REPO/target/release

# --- 3. Hardware ---------------------------------------------------------------
say "Detecting hardware"
"$BIN/thinkpower" capabilities | sed 's/^/    /' || die "This machine is not supported. Nothing was changed."

# --- 4. Config -------------------------------------------------------------------
if [[ -z $CONFIG_SRC && -f $CONFIG ]]; then
    say "Keeping existing $CONFIG"
else
    if [[ -z $CONFIG_SRC ]]; then
        model=$(cat /sys/class/dmi/id/product_version 2>/dev/null || true)
        CONFIG_SRC=$(grep -lF "model = \"$model\"" "$REPO"/configs/*.toml 2>/dev/null | head -n1 || true)
        if [[ -z $CONFIG_SRC ]]; then
            CONFIG_SRC=$REPO/configs/x1-nano-gen3.toml
            warn "no config for \"$model\" in configs/; starting from $(basename "$CONFIG_SRC"). Adjust it in the GUI."
        fi
    fi
    say "Config: $CONFIG_SRC"
    "$BIN/thinkpower" check "$CONFIG_SRC" | sed 's/^/    /' ||
        warn "some values don't fit this hardware. The daemon won't apply them until you fix them in the GUI."
fi

# --- 5. Old hand-made RAPL overrides in tuned ----------------------------------
# Profile dirs whose scripts write RAPL would fight the daemon, and same-named
# overrides hide Fedora's stock tuning. They are moved to a backup, not deleted.
OLD=()
for d in /etc/tuned/*/ /etc/tuned/profiles/*/; do
    d=${d%/}
    [[ -d $d && $d != /etc/tuned/profiles && $d != /etc/tuned/recommend.d ]] || continue
    if grep -qsr 'powercap/intel-rapl' "$d"; then OLD+=("$d"); fi
done
if [[ ${#OLD[@]} -gt 0 ]]; then
    say "Found tuned profile overrides that write power limits:"
    printf '    %s\n' "${OLD[@]}"
    echo "    They will be moved to $STATE/backup/ (restorable with ./uninstall.sh --restore-backup)."
    ask "Move them?" || die "Aborted. Nothing was changed."
fi

# --- 6. System install (root) ----------------------------------------------------
say "Installing (sudo)"
sudo env REPO="$REPO" BIN="$BIN" PREFIX="$PREFIX" LIBEXEC="$LIBEXEC" STATE="$STATE" CONFIG="$CONFIG" \
    CONFIG_SRC="$CONFIG_SRC" OLD="$(printf '%s\n' "${OLD[@]}")" bash -euo pipefail <<'ROOT'
ts=$(date +%Y%m%d-%H%M%S)
backup=$STATE/backup/$ts
mkdir -p "$backup"

if [[ -n $OLD ]]; then
    while IFS= read -r d; do
        [[ -n $d ]] || continue
        mkdir -p "$backup$(dirname "$d")"
        cp -a "$d" "$backup$d"
        rm -rf "$d"
        echo "$d" >> "$backup/moved.list"
        echo "    moved $d"
    done <<< "$OLD"
fi
[[ -f $CONFIG ]] && cp -a "$CONFIG" "$backup/config.toml"
rmdir "$backup" 2>/dev/null || true

files=(
    "$PREFIX/bin/thinkpower"
    "$LIBEXEC/thinkpowerd"
    /etc/systemd/system/thinkpowerd.service
    /etc/dbus-1/system.d/io.github.thinkpower.Daemon1.conf
    /usr/share/polkit-1/actions/io.github.thinkpower.policy
    "$PREFIX/share/applications/thinkpower.desktop"
    /etc/xdg/autostart/thinkpower-tray.desktop
    "$LIBEXEC/uninstall.sh"
)
install -Dm755 "$BIN/thinkpower" "$PREFIX/bin/thinkpower"
install -Dm755 "$BIN/thinkpowerd" "$LIBEXEC/thinkpowerd"
install -Dm755 "$REPO/uninstall.sh" "$LIBEXEC/uninstall.sh"
install -Dm644 "$REPO/packaging/thinkpowerd.service" /etc/systemd/system/thinkpowerd.service
install -Dm644 "$REPO/packaging/io.github.thinkpower.Daemon1.conf" /etc/dbus-1/system.d/io.github.thinkpower.Daemon1.conf
install -Dm644 "$REPO/packaging/io.github.thinkpower.policy" /usr/share/polkit-1/actions/io.github.thinkpower.policy
install -Dm644 "$REPO/packaging/thinkpower.desktop" "$PREFIX/share/applications/thinkpower.desktop"
install -Dm644 "$REPO/packaging/thinkpower-tray.desktop" /etc/xdg/autostart/thinkpower-tray.desktop
if [[ -n $CONFIG_SRC ]]; then
    install -Dm644 "$CONFIG_SRC" "$CONFIG"
fi
mkdir -p "$STATE"
printf '%s\n' "${files[@]}" > "$STATE/installed.list"

command -v restorecon >/dev/null && restorecon -RF "${files[@]}" /etc/thinkpower 2>/dev/null || true

systemctl daemon-reload
systemctl reload dbus 2>/dev/null || systemctl reload dbus-broker 2>/dev/null || true
if [[ -n $OLD ]]; then
    # Reload the now-stock tuned profiles.
    systemctl restart tuned
fi
systemctl enable thinkpowerd >/dev/null 2>&1
systemctl restart thinkpowerd
ROOT

# --- 7. Verify -----------------------------------------------------------------
# The tray starts on login (/etc/xdg/autostart); (re)start it now for this session.
if [[ -n ${DBUS_SESSION_BUS_ADDRESS:-} ]]; then
    pkill -u "$USER" -f '^/usr/local/bin/thinkpower tray$' 2>/dev/null || true
    sleep 0.5
    setsid "$PREFIX/bin/thinkpower" tray >/dev/null 2>&1 < /dev/null &
fi

say "Verifying"
sleep 2
if "$PREFIX/bin/thinkpower" verify | sed 's/^/    /'; then
    say "Done. Open \"ThinkPower\" from the app menu, or run: thinkpower"
else
    warn "installed, but the active profile was not applied cleanly. See: thinkpower status, journalctl -u thinkpowerd"
fi
