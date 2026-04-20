# ytsync-pi

Rust one-shot binary that syncs YouTube playlists and the Liked Videos list to a NAS. Audio sources become MP3s with MusicBrainz-enriched ID3 tags; video sources become MKVs in native YouTube codecs (no transcode). Designed to run alongside Pi-hole, Headscale and NAS services on a Raspberry Pi without being noticed.

## Status

v0.1.0 — scaffold only. `cargo check` passes; sync pipeline arrives in phase 2. See `.claude/build_phases.md`.

## How it works

- **Not a daemon.** A systemd timer wakes the binary on schedule; it runs one sync cycle and exits.
- **Strictly sequential** — one video at a time, one source at a time. Rate-limited downloads with human-like pauses.
- **Resource-capped** by systemd (CPU 25%, RAM 200M, IO idle). Zero footprint between runs.
- **State** in a small SQLite file, with yt-dlp's `--download-archive` as a second safety net.
- **Auth** via an exported Netscape cookies file. OAuth intentionally avoided.
- **Alerts** via ntfy, failures only.

## Commands

```
ytsync-pi run            # sync cycle (invoked by systemd timer)
ytsync-pi status         # last run summary + open failures
ytsync-pi test-cookies   # probe the cookies file
ytsync-pi show-config    # print resolved config
```

## Config

Copy `examples/config.example.toml` to `~/.config/ytsync-pi/config.toml` and edit. Schema is documented in `.claude/config_schema.md`.

## Dependencies (runtime)

- `yt-dlp`
- `ffmpeg`
- `fpcalc` (Chromaprint) — `apt install libchromaprint-tools` on Debian/Pi. Optional: without it, tracks still download with Tier-1 tags but no MusicBrainz enrichment.
- AcoustID API key — free at https://acoustid.org/new-application. Required to unlock MusicBrainz lookup.

## Build

```
cargo build --release
```

Release profile is tuned for a small binary (LTO, strip, `opt-level = "z"`, `panic = "abort"`).

## License

MIT
