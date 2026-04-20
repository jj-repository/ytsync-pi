#!/usr/bin/env bash
# Install ytsync-pi as a user-level systemd timer on a Pi (or any
# systemd-based host). No root required; the service runs as the invoking user.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &> /dev/null && pwd)"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
mkdir -p "$UNIT_DIR"

echo "Installing unit files into $UNIT_DIR"
install -m 0644 "$SCRIPT_DIR/ytsync-pi.service"         "$UNIT_DIR/ytsync-pi.service"
install -m 0644 "$SCRIPT_DIR/ytsync-pi.timer"           "$UNIT_DIR/ytsync-pi.timer"
install -m 0644 "$SCRIPT_DIR/ytsync-pi-canary.service"  "$UNIT_DIR/ytsync-pi-canary.service"
install -m 0644 "$SCRIPT_DIR/ytsync-pi-canary.timer"    "$UNIT_DIR/ytsync-pi-canary.timer"

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
