#!/usr/bin/env bash
# Install ytsync-pi as a user-level systemd timer on a Pi (or any
# systemd-based host). No root required; the service runs as the invoking user.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &> /dev/null && pwd)"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
mkdir -p "$UNIT_DIR"

FORCE=0
for arg in "$@"; do
    case "$arg" in
        -f|--force) FORCE=1 ;;
        -h|--help)
            cat <<USAGE
Usage: $(basename "$0") [--force]

  (no args)  Install fresh. Refuses to overwrite existing unit files so local
             edits (e.g. NAS mount path in ReadWritePaths) aren't silently
             reverted on a re-run.
  --force    Overwrite existing unit files unconditionally.
USAGE
            exit 0 ;;
        *) echo "unknown arg: $arg" >&2; exit 2 ;;
    esac
done

UNITS=(ytsync-pi.service ytsync-pi.timer ytsync-pi-canary.service ytsync-pi-canary.timer)

if [[ $FORCE -eq 0 ]]; then
    EXISTING=()
    for unit in "${UNITS[@]}"; do
        if [[ -e "$UNIT_DIR/$unit" ]] && ! cmp -s "$SCRIPT_DIR/$unit" "$UNIT_DIR/$unit"; then
            EXISTING+=("$unit")
        fi
    done
    if [[ ${#EXISTING[@]} -gt 0 ]]; then
        echo "Refusing to overwrite existing (possibly user-edited) unit files:" >&2
        for u in "${EXISTING[@]}"; do echo "  $UNIT_DIR/$u" >&2; done
        echo >&2
        echo "If you meant to replace them, re-run with --force." >&2
        echo "Otherwise, diff and merge changes by hand:" >&2
        echo "  diff -u $UNIT_DIR/${EXISTING[0]} $SCRIPT_DIR/${EXISTING[0]}" >&2
        exit 1
    fi
fi

echo "Installing unit files into $UNIT_DIR"
for unit in "${UNITS[@]}"; do
    install -m 0644 "$SCRIPT_DIR/$unit" "$UNIT_DIR/$unit"
done

echo "Reloading user systemd"
systemctl --user daemon-reload

echo "Enabling + starting timers"
systemctl --user enable --now ytsync-pi.timer
systemctl --user enable --now ytsync-pi-canary.timer

cat <<'EOF'

Done. Useful commands:

  systemctl --user list-timers                    # see next run time
  systemctl --user status ytsync-pi.timer
  systemctl --user status ytsync-pi.service
  journalctl --user -u ytsync-pi.service -f       # live log
  systemctl --user start ytsync-pi.service        # run now, ad-hoc

To keep the timer running while you are logged out (usually desired on a Pi):

  sudo loginctl enable-linger "$USER"

To uninstall:

  systemctl --user disable --now ytsync-pi.timer ytsync-pi-canary.timer
  rm -f "$UNIT_DIR"/ytsync-pi*.{service,timer}
  systemctl --user daemon-reload
EOF
