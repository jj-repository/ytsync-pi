use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;
use tracing::{info, warn};

use crate::config::{Config, Source, SourceMode};
use crate::db::{Db, ItemMode};
use crate::musicbrainz::{TagOutcome, Tagger};
use crate::ytdlp::{archive_path_for, DownloadResult, PlaylistEntry, YtDlp};
use crate::ytdlp_updater::YtDlpUpdater;

pub struct SyncStats {
    pub ok: u64,
    pub failed: u64,
    pub skipped_sources: u64,
    pub tagged: u64,
    pub tag_no_match: u64,
    pub tag_skipped: u64,
}

struct SyncCtx<'a> {
    cfg: &'a Config,
    db: &'a Db,
    yt: YtDlp<'a>,
    updater: &'a YtDlpUpdater,
    tagger: Option<Tagger>,
    archive: PathBuf,
    updated_this_run: bool,
    stats: SyncStats,
}

/// Orchestrates one sync cycle across all configured audio sources.
/// Video-mode sources are skipped with a warning until phase 5.
pub fn run_sync(cfg: &Config, db: &Db, updater: &YtDlpUpdater) -> SyncStats {
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
        stats: SyncStats {
            ok: 0,
            failed: 0,
            skipped_sources: 0,
            tagged: 0,
            tag_no_match: 0,
            tag_skipped: 0,
        },
    };

    for source in &cfg.sources {
        match source.mode {
            SourceMode::Audio => sync_audio_source(&mut ctx, source),
            SourceMode::Video => {
                warn!(
                    "source {:?} is video-mode; skipped (video pipeline lands in phase 5)",
                    source.name
                );
                ctx.stats.skipped_sources += 1;
            }
        }
    }
    ctx.stats
}

fn sync_audio_source(ctx: &mut SyncCtx, source: &Source) {
    info!("source {:?}: listing playlist", source.name);
    let entries = match ctx.yt.list_playlist(&source.url) {
        Ok(e) => e,
        Err(e) => {
            warn!(
                "source {:?}: listing failed ({}); stderr: {}",
                source.name,
                e.message,
                e.stderr.trim()
            );
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

    sync_entries(ctx, source, &entries);
}

fn sync_entries(ctx: &mut SyncCtx, source: &Source, entries: &[PlaylistEntry]) {
    let total = entries.len();
    let mut new_items: Vec<PlaylistEntry> = Vec::new();
    for entry in entries {
        match ctx.db.is_done(&entry.id, ItemMode::Audio) {
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
        match download_with_retries(ctx, source, &entry) {
            Ok(result) => {
                if let Err(e) = ctx.db.mark_done(
                    &entry.id,
                    &source.name,
                    ItemMode::Audio,
                    Some(&result.title),
                    &result.file_path,
                ) {
                    warn!("db mark_done failed for {}: {e}", entry.id);
                }
                info!("✓ {} → {}", entry.id, result.file_path.display());
                ctx.stats.ok += 1;
                enrich_tags(ctx, &result);
            }
            Err(err_msg) => {
                if let Err(e) =
                    ctx.db
                        .record_failure(&entry.id, &source.name, ItemMode::Audio, &err_msg)
                {
                    warn!("db record_failure failed for {}: {e}", entry.id);
                }
                warn!("✗ {}: {err_msg}", entry.id);
                ctx.stats.failed += 1;
            }
        }
    }
}

fn download_with_retries(
    ctx: &mut SyncCtx,
    source: &Source,
    entry: &PlaylistEntry,
) -> Result<DownloadResult, String> {
    let attempts = ctx.cfg.retries.saturating_add(1).max(1);
    let archive: &Path = &ctx.archive;
    let mut last_err = String::new();

    for attempt in 1..=attempts {
        let res = ctx
            .yt
            .download_audio(&entry.id, &ctx.cfg.output_audio_dir, archive);
        match res {
            Ok(r) => return Ok(r),
            Err(e) => {
                last_err = format!(
                    "attempt {attempt}/{attempts}: {} (stderr: {})",
                    e.message,
                    e.stderr.trim().chars().take(300).collect::<String>()
                );
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
    Err(last_err)
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
