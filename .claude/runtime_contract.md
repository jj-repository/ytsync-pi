# Runtime Contract

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Run completed (even if some items were skipped after retries) |
| 1 | Fatal error: config invalid, DB open failed, lock file IO error, panic |
| 2 | Another run already holds the lock (safe no-op; systemd should not treat as failure) |

systemd `SuccessExitStatus=2` so overlap is not alerted.

## Lock file

- Path: `lock_path` in config
- Acquired via `fs2::FileExt::try_lock_exclusive` (advisory POSIX lock)
- Removed on `Drop`; if the process dies abruptly, OS releases the lock automatically

## SQLite tables

- `items (video_id, mode, source_name, title, file_path, downloaded_at)` — primary key `(video_id, mode)`
- `failures (video_id, mode, source_name, last_error, attempts, last_attempt_at)` — cleared on success
- `runs (id, started_at, finished_at, ok_count, fail_count, notes)` — one row per invocation

PRAGMAs: `journal_mode=WAL`, `synchronous=NORMAL`, `foreign_keys=ON`.

## Failure handling

- Retries: `retries` attempts per item with `retry_backoff_sec` linear backoff
- After final attempt, item goes into `failures` and the run continues
- On next invocation, failed items are re-attempted (subject to the same cap per run)
- Run summary is always written to `runs`, even on partial failures
- Any `fail_count > 0` triggers a single ntfy notification at end of run

## Idempotency

- `yt-dlp --download-archive` records every successfully downloaded video by id — second safety net against duplicate downloads if SQLite gets wiped
- `items.video_id` is the authoritative dedupe key; `(video_id, mode)` composite allows the same video in both audio and video libraries if a user configures it
- File writes are atomic: `{tmp}.{mp3|mkv}.part` → tag → rename into `output_*_dir`
