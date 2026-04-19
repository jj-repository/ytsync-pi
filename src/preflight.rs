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
    Ok(PreflightReport { cookies_age_days })
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
    Ok(())
}

fn cookies_age_days(path: &Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let age = SystemTime::now().duration_since(mtime).ok()?;
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
