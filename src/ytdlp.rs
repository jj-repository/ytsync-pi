use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tracing::{debug, warn};
use wait_timeout::ChildExt;

use crate::config::Config;
use crate::ytdlp_updater::YtDlpUpdater;

/// A single entry returned by a flat playlist listing.
#[derive(Debug, Clone)]
pub struct PlaylistEntry {
    pub id: String,
    pub title: String,
}

/// Successful download summary.
#[derive(Debug, Clone)]
pub struct DownloadResult {
    pub file_path: PathBuf,
    pub title: String,
}

/// Error from a yt-dlp invocation, carrying enough context for retry logic.
#[derive(Debug)]
pub struct YtDlpError {
    pub message: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub looks_like_extractor: bool,
    pub looks_like_auth: bool,
    pub timed_out: bool,
}

impl std::fmt::Display for YtDlpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for YtDlpError {}

pub struct YtDlp<'a> {
    binary: PathBuf,
    cookies: PathBuf,
    cfg: &'a Config,
}

impl<'a> YtDlp<'a> {
    pub fn new(binary: PathBuf, cfg: &'a Config) -> Self {
        Self {
            binary,
            cookies: cfg.cookies_path.clone(),
            cfg,
        }
    }

    /// Flat-lists every entry in a playlist URL.
    pub fn list_playlist(&self, url: &str) -> std::result::Result<Vec<PlaylistEntry>, YtDlpError> {
        let mut cmd = Command::new(&self.binary);
        cmd.arg("--cookies")
            .arg(&self.cookies)
            .arg("--flat-playlist")
            .arg("--print")
            .arg("%(id)s\t%(title)s")
            .arg("--no-warnings")
            .arg("--")
            .arg(url)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let out = run_with_timeout(cmd, Duration::from_secs(300))?;

        if !out.exit_status.success() {
            return Err(YtDlpError {
                message: format!("playlist listing failed for {url}"),
                exit_code: out.exit_status.code(),
                looks_like_extractor: YtDlpUpdater::looks_like_extraction_failure(&out.stderr),
                looks_like_auth: YtDlpUpdater::looks_like_auth_failure(&out.stderr),
                stderr: out.stderr,
                timed_out: false,
            });
        }

        let mut entries = Vec::new();
        for line in out.stdout.lines() {
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            let (id, title) = match line.split_once('\t') {
                Some((id, title)) => (id.trim(), title.trim()),
                None => (line.trim(), ""),
            };
            if id.is_empty() || id == "NA" {
                continue;
            }
            entries.push(PlaylistEntry {
                id: id.to_string(),
                title: title.to_string(),
            });
        }
        Ok(entries)
    }

    /// Downloads one video id as MP3 into `dest_dir`.
    pub fn download_audio(
        &self,
        video_id: &str,
        dest_dir: &Path,
        archive_path: &Path,
    ) -> std::result::Result<DownloadResult, YtDlpError> {
        std::fs::create_dir_all(dest_dir).map_err(|e| YtDlpError {
            message: format!("create dest dir {}: {e}", dest_dir.display()),
            stderr: String::new(),
            exit_code: None,
            looks_like_extractor: false,
            looks_like_auth: false,
            timed_out: false,
        })?;
        if let Some(parent) = archive_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| YtDlpError {
                message: format!("create archive dir {}: {e}", parent.display()),
                stderr: String::new(),
                exit_code: None,
                looks_like_extractor: false,
                looks_like_auth: false,
                timed_out: false,
            })?;
        }

        let output_template = dest_dir.join("%(title)s.%(ext)s");
        let url = format!("https://www.youtube.com/watch?v={video_id}");

        let mut cmd = Command::new(&self.binary);
        cmd.arg("--cookies")
            .arg(&self.cookies)
            .arg("--download-archive")
            .arg(archive_path)
            .arg("--no-overwrites")
            .arg("--no-progress")
            .arg("--no-warnings")
            .arg("--retries")
            .arg("1") // we manage retries ourselves
            .arg("--fragment-retries")
            .arg("3")
            .arg("--limit-rate")
            .arg(&self.cfg.rate_limit)
            .arg("--sleep-interval")
            .arg(self.cfg.sleep_interval_sec.to_string())
            .arg("--max-sleep-interval")
            .arg(self.cfg.max_sleep_interval_sec.to_string())
            .arg("-f")
            .arg("bestaudio/best")
            .arg("-x")
            .arg("--audio-format")
            .arg("mp3")
            .arg("--audio-quality")
            .arg("0")
            .arg("--embed-metadata")
            .arg("--embed-thumbnail")
            .arg("--add-metadata")
            .arg("--print")
            .arg("after_move:%(filepath)s")
            .arg("-o")
            .arg(&output_template)
            .arg("--")
            .arg(&url)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let out = run_with_timeout(cmd, Duration::from_secs(self.cfg.per_item_timeout_sec))?;

        if !out.exit_status.success() {
            return Err(YtDlpError {
                message: format!("download failed for {video_id}"),
                exit_code: out.exit_status.code(),
                looks_like_extractor: YtDlpUpdater::looks_like_extraction_failure(&out.stderr),
                looks_like_auth: YtDlpUpdater::looks_like_auth_failure(&out.stderr),
                stderr: out.stderr,
                timed_out: false,
            });
        }

        // `--print after_move:%(filepath)s` writes exactly one line per download
        // with the final on-disk path.
        let file_path = out
            .stdout
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(|l| PathBuf::from(l.trim()))
            .ok_or_else(|| YtDlpError {
                message: format!("yt-dlp reported success but emitted no filepath for {video_id}"),
                stderr: out.stderr.clone(),
                exit_code: out.exit_status.code(),
                looks_like_extractor: false,
                looks_like_auth: false,
                timed_out: false,
            })?;

        let title = file_path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| video_id.to_string());

        Ok(DownloadResult { file_path, title })
    }
}

struct InvocationOutput {
    exit_status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

fn run_with_timeout(
    mut cmd: Command,
    timeout: Duration,
) -> std::result::Result<InvocationOutput, YtDlpError> {
    debug!("spawning yt-dlp: {cmd:?}");
    let mut child = cmd.spawn().map_err(|e| YtDlpError {
        message: format!("spawn yt-dlp: {e}"),
        stderr: String::new(),
        exit_code: None,
        looks_like_extractor: false,
        looks_like_auth: false,
        timed_out: false,
    })?;

    let status = match child.wait_timeout(timeout) {
        Ok(Some(s)) => s,
        Ok(None) => {
            warn!("yt-dlp exceeded {}s timeout; killing", timeout.as_secs());
            let _ = child.kill();
            let _ = child.wait();
            return Err(YtDlpError {
                message: format!("yt-dlp timed out after {}s", timeout.as_secs()),
                stderr: String::new(),
                exit_code: None,
                looks_like_extractor: false,
                looks_like_auth: false,
                timed_out: true,
            });
        }
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(YtDlpError {
                message: format!("wait_timeout error: {e}"),
                stderr: String::new(),
                exit_code: None,
                looks_like_extractor: false,
                looks_like_auth: false,
                timed_out: false,
            });
        }
    };

    let out = child.wait_with_output().map_err(|e| YtDlpError {
        message: format!("wait_with_output: {e}"),
        stderr: String::new(),
        exit_code: None,
        looks_like_extractor: false,
        looks_like_auth: false,
        timed_out: false,
    })?;

    Ok(InvocationOutput {
        exit_status: status,
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    })
}

/// Resolves the archive-file path for yt-dlp's `--download-archive`.
/// Parked next to the SQLite DB so both sources of truth share a parent dir.
pub fn archive_path_for(cfg: &Config) -> Result<PathBuf> {
    let parent = cfg
        .db_path
        .parent()
        .context("db_path has no parent directory")?;
    Ok(parent.join("ytdlp-archive.txt"))
}
