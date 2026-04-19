# Config Schema

File: `~/.config/ytsync-pi/config.toml` (override with `-c <path>`). Paths support `~` and `$VAR` expansion.

## Top-level

| Key | Type | Default | Purpose |
|-----|------|---------|---------|
| `cookies_path` | path | required | Netscape cookies file for yt-dlp `--cookies` |
| `output_audio_dir` | path | required | MP3 destination (NAS mount) |
| `output_video_dir` | path | required | MKV destination (NAS mount) |
| `db_path` | path | required | SQLite state file |
| `lock_path` | path | required | flock file to prevent overlapping runs |
| `bitrate` | string | `"320k"` | MP3 bitrate (`--audio-quality 0` maps to 320 CBR) |
| `rate_limit` | string | `"2M"` | yt-dlp `--limit-rate` |
| `retries` | int | `3` | Per-item retry count before skip |
| `retry_backoff_sec` | int | `60` | Seconds between retries (linear) |
| `sleep_interval_sec` | int | `5` | yt-dlp `--sleep-interval` (min pause between items) |
| `max_sleep_interval_sec` | int | `15` | yt-dlp `--max-sleep-interval` |
| `per_item_timeout_sec` | int | `1800` | Hard timeout per video |
| `min_free_disk_gb` | int | `2` | Abort if target mount has less free |

## `[ntfy]`

| Key | Type | Purpose |
|-----|------|---------|
| `server` | string | ntfy server URL (default `https://ntfy.sh`) |
| `topic` | string | Topic name, e.g. `ytsync-pi-alerts` |

## `[musicbrainz]`

| Key | Type | Purpose |
|-----|------|---------|
| `acoustid_api_key` | string | From https://acoustid.org/new-application |
| `enabled` | bool | Set `false` to skip Tier 2 enrichment |

## `[[sources]]` (array)

| Key | Type | Purpose |
|-----|------|---------|
| `name` | string | Human label, used in logs and SQLite |
| `url` | string | Playlist URL — `list=LL` for Liked Videos |
| `mode` | `"audio"` \| `"video"` | Output pipeline |
| `quality` | string? | Video-mode only, e.g. `"1080p"` (optional cap) |
