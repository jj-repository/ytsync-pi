# Build Phases

1. **Bootstrap** — Rust binary, TOML config, SQLite schema, lock file, CLI skeleton. ✅ **Done 2026-04-19.**
2. **yt-dlp updater** — pre-run mtime check against `update_if_older_than_days`, self-update via `yt-dlp -U --update-to <channel>` (nightly default), version probe logged to every run, manual `update-ytdlp` subcommand. Failed update is non-fatal; fall through to the existing binary.
3. **Core sync** — yt-dlp subprocess wrapper, one playlist audio end-to-end, MP3 output with Tier-1 (yt-dlp embed-metadata) tags. Extraction-failure retry triggers a yt-dlp update + single retry before giving up.
4. **MusicBrainz tagging** — Chromaprint (`fpcalc`) → AcoustID → MusicBrainz API → ID3 enrichment. 1 req/sec MB rate limit.
5. **Video mode + retries** — `mode = "video"` sources, MKV output (`bv*+ba/b`, `--merge-output-format mkv`), per-item retry/backoff, failure table.
6. **systemd unit + timer + hardening** — drop-in `.service`/`.timer`, resource caps, `RequiresMountsFor=`, journald logging.
7. **Alerts + introspection** — ntfy on failures, `status` summary, `test-cookies` live probe.
8. **Deploy + tune** — install on Pi, observe real load alongside Pi-hole/Headscale/NAS stack, tune caps.

## Decisions locked 2026-04-19

- Auth: **cookies** (yt-dlp `--cookies`), not OAuth.
- Concurrency: **sequential only**, one video / one playlist at a time.
- Video: **MKV** with native VP9/AV1 + Opus (no transcode — Pi can't keep 24/7-quiet budget while re-encoding).
- Audio: **MP3 320 kbps CBR** via yt-dlp `--audio-quality 0`.
- Metadata: Tier 1 (yt-dlp) + Tier 2 (MusicBrainz) from MVP.
- Filename: `{title}.(mp3|mkv)` with sanitization.
- Alerts: ntfy topic `ytsync-pi-alerts`, failures only.
- **yt-dlp install:** standalone binary (not pip/apt), path configurable (default `~/.local/bin/yt-dlp`).
- **yt-dlp channel:** `nightly` by default — YouTube-dedicated workloads benefit from early extractor fixes.
- **yt-dlp update triggers:** (a) binary mtime > 3 days before a run, (b) extraction-signature errors during a run trigger one update + one retry, (c) manual `update-ytdlp` subcommand.
- **yt-dlp version logging:** printed at start of every run and saved into `runs.notes`.
