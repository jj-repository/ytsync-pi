# Architecture

## One-shot binary, timer-driven

The binary exits after every run. systemd's timer wakes it on schedule. No resident daemon → zero idle memory, no leak risk. `Restart=no` in the unit — the timer is the loop.

## Components

```
systemd.timer ──► ytsync-pi run ──► acquire flock ──► load config ──► open SQLite
                                                                     │
                                                                     ▼
                                                        for each source (sequential):
                                                          list playlist via yt-dlp
                                                          diff against SQLite + archive
                                                          for each new id (sequential):
                                                            download (audio|video)
                                                            (audio) fingerprint → AcoustID → MB → ID3
                                                            move to NAS path atomically
                                                            record in SQLite
                                                          on error: retry w/ backoff, else log+skip
                                                                     │
                                                                     ▼
                                                                  summary → journald
                                                                  on any failures → ntfy
```

## Resource discipline

- systemd caps: `CPUQuota=25%`, `MemoryMax=200M`, `MemorySwapMax=0`, `IOSchedulingClass=idle`, `Nice=19`, `TasksMax=50`
- yt-dlp network: `--limit-rate 2M`, `--sleep-interval 5 --max-sleep-interval 15`
- Off-peak window: `OnCalendar=03:30`, `RandomizedDelaySec=30min`
- Disk guard: skip download if free space on target mount < `min_free_disk_gb`
- Per-item timeout: `per_item_timeout_sec` (default 1800)

## Stability guards

- `flock` on `lock_path` prevents overlapping runs
- SQLite WAL + `synchronous=NORMAL` for safe concurrent reads during `status`
- Atomic writes: download to temp path, rename into place after tagging
- NAS mount check (`RequiresMountsFor=` in systemd unit) — abort clean if the share vanishes
- Panic → ntfy alert
- SIGTERM → finish current item, then exit

## Not a daemon

Deliberately avoids long-running process patterns: no in-process scheduler, no cached connections, no background threads surviving across runs. Every invocation is independent and idempotent.
