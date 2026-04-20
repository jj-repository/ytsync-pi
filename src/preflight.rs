use anyhow::{anyhow, Context, Result};
use fs2::available_space;
use std::path::Path;
use std::time::{Duration, SystemTime};
use tracing::{info, warn};

use crate::config::Config;

pub struct PreflightReport {
    pub cookies_age_days: Option<u64>,
}

/// Runs pre-sync checks that must succeed before touching yt-dlp or the DB.
/// Bails on hard failures (missing cookies, unwritable output dir, low disk).
pub fn run(cfg: &Config) -> Result<PreflightReport> {
    check_cookies(cfg)?;
    let cookies_age_days = cookies_age_days(&cfg.cookies_path);
    check_output_dirs(cfg)?;
    check_free_space(cfg)?;
    sweep_partial_files(cfg);
    Ok(PreflightReport { cookies_age_days })
}

/// yt-dlp leaves `.part` (and occasionally `.ytdl`) files in the output dir if
/// a download is interrupted — SIGKILL on timeout, OOM-kill, power loss. They
/// are never resumed on the next run, so they just accumulate. Best-effort
/// cleanup at sync start; non-fatal if it fails.
fn sweep_partial_files(cfg: &Config) {
    for dir in [&cfg.output_audio_dir, &cfg.output_video_dir] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.ends_with(".part") || name.ends_with(".ytdl") {
                match std::fs::remove_file(&path) {
                    Ok(_) => info!("swept partial: {}", path.display()),
                    Err(e) => warn!("could not sweep {}: {e}", path.display()),
                }
            }
        }
    }
}

fn check_cookies(cfg: &Config) -> Result<()> {
    if !cfg.cookies_path.exists() {
        return Err(anyhow!(
            "cookies file missing at {}. Export via your browser (yt-dlp-compatible Netscape format).",
            cfg.cookies_path.display()
        ));
    }
    let meta = std::fs::metadata(&cfg.cookies_path)
        .with_context(|| format!("stat cookies {}", cfg.cookies_path.display()))?;
    if meta.len() == 0 {
        return Err(anyhow!(
            "cookies file is empty: {}",
            cfg.cookies_path.display()
        ));
    }
    // The cookies file holds live YouTube session tokens. Anything readable by
    // group or other is a leak — bail loudly rather than silently proceeding.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(anyhow!(
                "cookies file {} has permissions {:o}; must be 0600 (run `chmod 600 {}`)",
                cfg.cookies_path.display(),
                mode,
                cfg.cookies_path.display()
            ));
        }
    }
    Ok(())
}

fn cookies_age_days(path: &Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    // `duration_since` errors when `now < mtime`, which happens on a Pi booted
    // without NTP sync (no RTC). Saturate to zero so the warning still evaluates
    // against a real age once the clock catches up.
    let age = SystemTime::now()
        .duration_since(mtime)
        .unwrap_or(Duration::ZERO);
    let days = age.as_secs() / 86_400;
    if age > Duration::from_secs(30 * 86_400) {
        warn!(
            "cookies file is {days} days old; YouTube may invalidate it soon — consider re-exporting"
        );
    }
    Some(days)
}

fn check_output_dirs(cfg: &Config) -> Result<()> {
    for dir in [&cfg.output_audio_dir, &cfg.output_video_dir] {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create output dir {}", dir.display()))?;
        let probe = dir.join(".ytsync-pi-write-probe");
        std::fs::write(&probe, b"")
            .with_context(|| format!("output dir {} is not writable", dir.display()))?;
        let _ = std::fs::remove_file(&probe);
    }
    Ok(())
}

fn check_free_space(cfg: &Config) -> Result<()> {
    for dir in [&cfg.output_audio_dir, &cfg.output_video_dir] {
        let bytes = available_space(dir)
            .with_context(|| format!("query free space on {}", dir.display()))?;
        let gb = bytes / 1_073_741_824;
        if gb < cfg.min_free_disk_gb {
            return Err(anyhow!(
                "only {gb} GB free on {} (need {} GB)",
                dir.display(),
                cfg.min_free_disk_gb
            ));
        }
        info!("free space on {}: {gb} GB", dir.display());
    }
    Ok(())
}
