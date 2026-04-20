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
- Wall-clock budget: `per_item_timeout_sec` applies across all attempts for the item, not per attempt — retries cannot blow past that budget
- After final attempt, item goes into `failures` (full stderr is persisted there for later inspection) and the run continues
- `failures` rows older than 90 days are pruned at the start of each run
- Partial `.part` / `.ytdl` files in the output dirs are swept at preflight — yt-dlp never resumes them on the next run, so they would otherwise accumulate
- On next invocation, failed items are re-attempted (subject to the same cap per run)
- If `--download-archive` already has a video id but the DB does not (e.g. a previous run crashed between archive write and DB commit), the next run detects the drift and reconciles the DB row without re-downloading
- SIGTERM from systemd breaks between items; the run row is always finalized with an `aborted=true` note rather than left with `finished_at=NULL`
- Run summary is always written to `runs`, even on partial failures
- Any `fail_count > 0` triggers a single ntfy notification at end of run (priority 3)
- A sticky `cookies_suspicious` flag on the run row triggers a separate, higher-priority ntfy alert (priority 4) so cookie-expiration is surfaced even in runs where download failures are otherwise few
- ntfy failures are logged but never escalate — the run itself has already been recorded in SQLite

## Idempotency

- `yt-dlp --download-archive` records every successfully downloaded video by id — second safety net against duplicate downloads if SQLite gets wiped
- `items.video_id` is the authoritative dedupe key; `(video_id, mode)` composite allows the same video in both audio and video libraries if a user configures it
- File writes are atomic: `{tmp}.{mp3|mkv}.part` → tag → rename into `output_*_dir`
