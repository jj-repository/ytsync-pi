use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;
use tracing::{info, warn};

use crate::config::{Config, Source, SourceMode};
use crate::db::{Db, ItemMode};
use crate::musicbrainz::{TagOutcome, Tagger};
use crate::shutdown::ShutdownFlag;
use crate::ytdlp::{archive_path_for, DownloadResult, PlaylistEntry, YtDlp};
use crate::ytdlp_updater::YtDlpUpdater;

pub struct SyncStats {
    pub ok: u64,
    pub failed: u64,
    pub skipped_sources: u64,
    pub tagged: u64,
    pub tag_no_match: u64,
    pub tag_skipped: u64,
    pub cookies_suspicious: bool,
}

struct SyncCtx<'a> {
    cfg: &'a Config,
    db: &'a Db,
    yt: YtDlp<'a>,
    updater: &'a YtDlpUpdater,
    tagger: Option<Tagger>,
    archive: PathBuf,
    updated_this_run: bool,
    shutdown: &'a ShutdownFlag,
    stats: SyncStats,
}

/// Orchestrates one sync cycle across every configured source, audio and video.
pub fn run_sync(
    cfg: &Config,
    db: &Db,
    updater: &YtDlpUpdater,
    shutdown: &ShutdownFlag,
) -> SyncStats {
    let archive = match archive_path_for(cfg) {
        Ok(p) => p,
        Err(e) => {
            warn!("could not resolve yt-dlp archive path: {e}; aborting sync");
            return SyncStats {
                ok: 0,
                failed: 1,
                skipped_sources: 0,
                tagged: 0,
                tag_no_match: 0,
                tag_skipped: 0,
                cookies_suspicious: false,
            };
        }
    };

    let tagger = cfg.musicbrainz.as_ref().map(|m| Tagger::new(m.clone()));
    let mut ctx = SyncCtx {
        cfg,
        db,
        yt: YtDlp::new(updater.binary_path(), cfg),
        updater,
        tagger,
        archive,
        updated_this_run: false,
        shutdown,
        stats: SyncStats {
            ok: 0,
            failed: 0,
            skipped_sources: 0,
            tagged: 0,
            tag_no_match: 0,
            tag_skipped: 0,
            cookies_suspicious: false,
        },
    };

    for source in &cfg.sources {
        if ctx.shutdown.is_set() {
            warn!("shutdown requested; skipping remaining sources");
            break;
        }
        let mode = match source.mode {
            SourceMode::Audio => ItemMode::Audio,
            SourceMode::Video => ItemMode::Video,
        };
        sync_source(&mut ctx, source, mode);
    }
    ctx.stats
}

fn sync_source(ctx: &mut SyncCtx, source: &Source, mode: ItemMode) {
    info!(
        "source {:?} ({:?}): listing playlist",
        source.name, source.mode
    );
    let entries = match ctx.yt.list_playlist(&source.url) {
        Ok(e) => e,
        Err(e) => {
            warn!(
                "source {:?}: listing failed ({}); stderr: {}",
                source.name,
                e.message,
                e.stderr.trim()
            );
            if e.looks_like_auth {
                flag_cookies_suspicious(ctx, &source.name, &e.stderr);
            }
            if e.looks_like_extractor
                && !ctx.updated_this_run
                && ctx.cfg.yt_dlp.update_on_extract_error
            {
                info!("listing error looks like an extractor failure; forcing yt-dlp update");
                let _ = ctx.updater.update_now();
                ctx.updated_this_run = true;
                match ctx.yt.list_playlist(&source.url) {
                    Ok(e) => e,
                    Err(e2) => {
                        warn!(
                            "source {:?}: listing still failed after update: {}",
                            source.name, e2.message
                        );
                        ctx.stats.failed += 1;
                        return;
                    }
                }
            } else {
                ctx.stats.failed += 1;
                return;
            }
        }
    };

    sync_entries(ctx, source, mode, &entries);
}

fn sync_entries(ctx: &mut SyncCtx, source: &Source, mode: ItemMode, entries: &[PlaylistEntry]) {
    let total = entries.len();
    let mut new_items: Vec<PlaylistEntry> = Vec::new();
    for entry in entries {
        match ctx.db.is_done(&entry.id, mode) {
            Ok(true) => {}
            Ok(false) => new_items.push(entry.clone()),
            Err(e) => warn!("db check failed for {}: {e}", entry.id),
        }
    }
    info!(
        "source {:?}: {} entries total, {} new",
        source.name,
        total,
        new_items.len()
    );

    for entry in new_items {
        if ctx.shutdown.is_set() {
            warn!(
                "shutdown requested; stopping source {:?} after {} ok / {} fail",
                source.name, ctx.stats.ok, ctx.stats.failed
            );
            break;
        }
        match download_with_retries(ctx, source, mode, &entry) {
            Ok(Some(result)) => {
                if let Err(e) = ctx.db.mark_done(
                    &entry.id,
                    &source.name,
                    mode,
                    Some(&result.title),
                    &result.file_path,
                ) {
                    warn!("db mark_done failed for {}: {e}", entry.id);
                }
                info!("✓ {} → {}", entry.id, result.file_path.display());
                ctx.stats.ok += 1;
                if mode == ItemMode::Audio {
                    enrich_tags(ctx, &result);
                }
            }
            Ok(None) => {
                // yt-dlp archive already had this id but our DB did not — a
                // previous run recorded the archive entry then failed to
                // commit the DB row. Reconcile silently.
                info!(
                    "↺ {}: already in yt-dlp archive; reconciling DB entry",
                    entry.id
                );
                let placeholder = std::path::PathBuf::from("(archive-hit)");
                if let Err(e) = ctx
                    .db
                    .mark_done(&entry.id, &source.name, mode, None, &placeholder)
                {
                    warn!("db mark_done failed for {}: {e}", entry.id);
                }
                ctx.stats.ok += 1;
            }
            Err(err) => {
                if let Err(e) = ctx
                    .db
                    .record_failure(&entry.id, &source.name, mode, &err.full)
                {
                    warn!("db record_failure failed for {}: {e}", entry.id);
                }
                warn!("✗ {}: {}", entry.id, err.display);
                ctx.stats.failed += 1;
            }
        }
    }
}

pub struct AttemptError {
    pub display: String, // short, safe to log
    pub full: String,    // full stderr for forensic storage in `failures`
}

fn download_with_retries(
    ctx: &mut SyncCtx,
    source: &Source,
    mode: ItemMode,
    entry: &PlaylistEntry,
) -> Result<Option<DownloadResult>, AttemptError> {
    let attempts = ctx.cfg.retries.saturating_add(1).max(1);
    let archive: PathBuf = ctx.archive.clone();
    let output_dir: PathBuf = match mode {
        ItemMode::Audio => ctx.cfg.output_audio_dir.clone(),
        ItemMode::Video => ctx.cfg.output_video_dir.clone(),
    };
    let quality_cap = source.quality.clone();
    let mut last_display = String::new();
    let mut last_full = String::new();
    let item_start = std::time::Instant::now();
    let item_budget = Duration::from_secs(ctx.cfg.per_item_timeout_sec);

    for attempt in 1..=attempts {
        if ctx.shutdown.is_set() {
            return Err(AttemptError {
                display: format!("attempt {attempt}/{attempts}: shutdown requested"),
                full: "shutdown requested before attempt".to_string(),
            });
        }
        // Enforce the per-item wall-clock budget across all attempts, not just
        // per attempt. Prevents `retries × per_item_timeout` from bloating the
        // service's total runtime.
        let elapsed = item_start.elapsed();
        if elapsed >= item_budget {
            return Err(AttemptError {
                display: format!(
                    "item budget of {}s exhausted before attempt {attempt}",
                    item_budget.as_secs()
                ),
                full: last_full.clone(),
            });
        }

        let res = match mode {
            ItemMode::Audio => ctx.yt.download_audio(&entry.id, &output_dir, &archive),
            ItemMode::Video => {
                ctx.yt
                    .download_video(&entry.id, &output_dir, &archive, quality_cap.as_deref())
            }
        };
        match res {
            Ok(r) => return Ok(r),
            Err(e) => {
                let snippet: String = e.stderr.trim().chars().take(300).collect();
                last_display = format!(
                    "attempt {attempt}/{attempts}: {} (stderr: {})",
                    e.message, snippet
                );
                last_full = format!(
                    "attempt {attempt}/{attempts}: {}\n--- stderr ---\n{}",
                    e.message,
                    e.stderr.trim(),
                );
                if e.looks_like_auth {
                    flag_cookies_suspicious(ctx, &source.name, &e.stderr);
                }
                if e.looks_like_extractor
                    && !ctx.updated_this_run
                    && ctx.cfg.yt_dlp.update_on_extract_error
                {
                    info!(
                        "source {:?}: extractor failure on {}; forcing yt-dlp update",
                        source.name, entry.id
                    );
                    let _ = ctx.updater.update_now();
                    ctx.updated_this_run = true;
                    continue;
                }
                if attempt < attempts {
                    sleep(Duration::from_secs(ctx.cfg.retry_backoff_sec));
                }
            }
        }
    }
    Err(AttemptError {
        display: last_display,
        full: last_full,
    })
}

/// Records that at least one yt-dlp call in this run returned a
/// cookies/auth-rejection signature. We only log the loud banner once per run
/// so journald stays readable, but the sticky flag lives on through
/// `finish_run` and a phase-7 ntfy alert.
fn flag_cookies_suspicious(ctx: &mut SyncCtx, source_name: &str, stderr: &str) {
    if !ctx.stats.cookies_suspicious {
        warn!(
            "🍪 cookies likely expired (source {:?}) — re-export your browser cookies. stderr snippet: {}",
            source_name,
            stderr.trim().chars().take(200).collect::<String>()
        );
    }
    ctx.stats.cookies_suspicious = true;
}

fn enrich_tags(ctx: &mut SyncCtx, result: &DownloadResult) {
    let Some(tagger) = ctx.tagger.as_ref() else {
        return;
    };
    if !tagger.enabled() {
        return;
    }
    match tagger.tag_mp3(&result.file_path) {
        TagOutcome::Enriched(tags) => {
            ctx.stats.tagged += 1;
            info!(
                "  tagged: artist={:?} album={:?} year={:?} genres={}",
                tags.artist,
                tags.album,
                tags.year,
                tags.genres.join(",")
            );
        }
        TagOutcome::NoMatch => {
            ctx.stats.tag_no_match += 1;
            info!("  no MusicBrainz match for {}", result.file_path.display());
        }
        TagOutcome::Skipped(reason) => {
            ctx.stats.tag_skipped += 1;
            warn!(
                "  tagging skipped for {}: {reason}",
                result.file_path.display()
            );
        }
    }
}
