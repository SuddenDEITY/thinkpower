#!/usr/bin/env bash
# Remove ThinkPower and return power profiles to Fedora's stock tuned behaviour.
#
#   ./uninstall.sh                    # remove; keep backups in /var/lib/thinkpower
#   ./uninstall.sh --restore-backup   # also put back the tuned overrides moved at install
#   ./uninstall.sh --yes              # don't ask (used by the GUI via pkexec)
set -euo pipefail
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

YES=0
RESTORE_BACKUP=0
for a in "$@"; do
    case $a in
        --yes|-y) YES=1 ;;
        --restore-backup) RESTORE_BACKUP=1 ;;
        -h|--help) sed -n '2,6p' "$0"; exit 0 ;;
        *) echo "unknown argument: $a" >&2; exit 2 ;;
    esac
done

if [[ $EUID -ne 0 ]]; then
    exec sudo "$0" "$@"
fi

STATE=/var/lib/thinkpower
if [[ $YES != 1 ]]; then
    read -r -p "Remove ThinkPower and restore default power profile behaviour? [y/N] " a
    [[ $a == [yY]* ]] || exit 1
fi

systemctl disable --now thinkpowerd 2>/dev/null || true
pkill -f '^/usr/local/bin/thinkpower tray$' 2>/dev/null || true

mkdir -p "$STATE"
if [[ -f /etc/thinkpower/config.toml ]]; then
    cp -a /etc/thinkpower/config.toml "$STATE/last-config.toml"
    echo "Saved your config to $STATE/last-config.toml"
fi
rm -f /etc/thinkpower/config.toml /etc/thinkpower/.config.toml.tmp
rmdir /etc/thinkpower 2>/dev/null || true

if [[ -f $STATE/installed.list ]]; then
    mapfile -t files < "$STATE/installed.list"
else
    files=(/usr/local/bin/thinkpower /usr/local/libexec/thinkpower/thinkpowerd
        /etc/systemd/system/thinkpowerd.service /etc/dbus-1/system.d/io.github.thinkpower.Daemon1.conf
        /usr/share/polkit-1/actions/io.github.thinkpower.policy /usr/local/share/applications/thinkpower.desktop
        /etc/xdg/autostart/thinkpower-tray.desktop
        /usr/local/libexec/thinkpower/uninstall.sh)
fi
for f in "${files[@]}"; do
    rm -f "$f"
done
rmdir /usr/local/libexec/thinkpower 2>/dev/null || true
rm -f "$STATE/installed.list"

if [[ $RESTORE_BACKUP == 1 ]]; then
    latest=$(find "$STATE/backup" -mindepth 1 -maxdepth 1 -type d -exec test -f '{}/moved.list' ';' -print 2>/dev/null | sort | tail -n1)
    if [[ -n $latest ]]; then
        while IFS= read -r d; do
            [[ -n $d ]] || continue
            mkdir -p "$(dirname "$d")"
            cp -a "$latest$d" "$d"
            echo "Restored $d"
        done < "$latest/moved.list"
    else
        echo "No backup with moved tuned profiles found."
    fi
fi

systemctl daemon-reload
systemctl reload dbus 2>/dev/null || true
systemctl restart tuned 2>/dev/null || true

echo "ThinkPower removed. Active tuned profile: $(tuned-adm active 2>/dev/null | sed 's/^Current active profile: //')"
echo "CPU power limits keep their current values until the next reboot."
echo "Backups remain in $STATE/backup."
