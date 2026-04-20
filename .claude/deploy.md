# Deploy (Pi)

Ships as a user-level systemd timer. No root needed for the service itself;
`loginctl enable-linger` is the only privileged step and only if you want the
timer to keep firing while logged out (usually yes on a Pi).

## Prerequisites on the Pi

```
sudo apt update
sudo apt install -y ffmpeg libchromaprint-tools curl
mkdir -p ~/.local/bin
curl -L https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp \
  -o ~/.local/bin/yt-dlp
chmod +x ~/.local/bin/yt-dlp
```

- `ffmpeg` — yt-dlp's audio extraction + MKV muxing
- `libchromaprint-tools` — provides `fpcalc` for MusicBrainz fingerprinting
- `yt-dlp` — managed by ytsync-pi's updater after first run

## Binary

Build on a dev host (cross-compile for `aarch64-unknown-linux-gnu`) or build
on the Pi directly:

```
cargo build --release
install -m 0755 target/release/ytsync-pi ~/.local/bin/ytsync-pi
```

## Config

```
mkdir -p ~/.config/ytsync-pi
cp examples/config.example.toml ~/.config/ytsync-pi/config.toml
$EDITOR ~/.config/ytsync-pi/config.toml
```

Required edits:

- `cookies_path` → export Netscape-format cookies from your browser into that path (e.g. via the `cookies.txt` browser extension)
- `output_audio_dir` / `output_video_dir` → your NAS mount paths
- `musicbrainz.acoustid_api_key` → register once at https://acoustid.org/new-application

Lock cookies down:

```
chmod 600 ~/.config/ytsync-pi/cookies.txt
```

## Install the timers

```
./systemd/install.sh
sudo loginctl enable-linger "$USER"
```

This installs `ytsync-pi.timer` (daily 03:30 + jitter) and
`ytsync-pi-canary.timer` (Sunday 10:00 + jitter — probes cookies so expiration
is caught even during weeks with no downloads).

## Edit for your NAS path

If your NAS mount is not at `/mnt/nas`, edit
`~/.config/systemd/user/ytsync-pi.service`:

- `RequiresMountsFor=` — uncomment and set to the mount root
- `ReadWritePaths=` — replace `/mnt/nas` with your mount

Then `systemctl --user daemon-reload && systemctl --user restart ytsync-pi.timer`.

## Observability

```
systemctl --user list-timers
systemctl --user status ytsync-pi.service
journalctl --user -u ytsync-pi.service -f
ytsync-pi status               # DB-backed run summary + cookies flag
ytsync-pi test-ntfy            # confirm alerts reach your device
```

## Alerts

Set `[ntfy]` in `config.toml`:

```
[ntfy]
server  = "https://ntfy.sh"
topic   = "ytsync-pi-alerts"
enabled = true
# token = "tk_…"   # optional, for protected topics
```

Subscribe on your phone/desktop to that topic. Two alert types fire:

- **priority 3 (default)** — one per run with `fail_count > 0`
- **priority 4 (warning)** — whenever yt-dlp stderr matches auth/cookie signatures (separate from download-failures so cookie expiration is never masked)

Send a one-off test with `ytsync-pi test-ntfy`.

## Resource ceiling

The service runs with:

- `CPUQuota=25%` · `MemoryMax=200M` · `MemorySwapMax=0`
- `IOSchedulingClass=idle` · `Nice=19` · `TasksMax=50`

These are intentionally tight so ytsync-pi does not disturb Pi-hole /
Headscale / NAS workloads. If runs start OOM-killing on very large
playlists, bump `MemoryMax` to `256M` first — Python-backed yt-dlp is the
usual culprit.

## Uninstall

```
systemctl --user disable --now ytsync-pi.timer ytsync-pi-canary.timer
rm -f ~/.config/systemd/user/ytsync-pi{,-canary}.{service,timer}
systemctl --user daemon-reload
```
