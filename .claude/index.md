# ytsync-pi — Project Index

Rust daemon for a Raspberry Pi that syncs YouTube playlists / Liked Videos to a NAS. Audio sources become MP3s with MusicBrainz-derived ID3 tags; video sources become MKVs in native YouTube codecs (no transcode).

## Topic files

- [Architecture](architecture.md) — one-shot binary pattern, schedule, resource caps, data flow
- [Build Phases](build_phases.md) — phased roadmap and current status
- [Config Schema](config_schema.md) — TOML fields, defaults, source modes
- [Runtime Contract](runtime_contract.md) — lock, DB, exit codes, failure handling
- [Deploy](deploy.md) — Pi install, systemd units, resource caps, uninstall

## Quick facts

- **Invocation:** `ytsync-pi run` (systemd timer), `status`, `test-cookies`, `show-config`
- **Config path:** `~/.config/ytsync-pi/config.toml` (XDG), override with `-c`
- **State:** SQLite at `~/.local/share/ytsync-pi/state.db`, yt-dlp archive as double-check
- **Auth:** cookies file (`--cookies`), **not** OAuth
- **Concurrency:** strictly sequential — one video at a time, one source at a time
- **Schedule:** systemd timer, 03:30 daily, `Persistent=true`, `RandomizedDelaySec=30min`
- **Alerts:** ntfy topic `ytsync-pi-alerts`, failures only
