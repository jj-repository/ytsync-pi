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
    ///
    /// Returns `Ok(Some(..))` for a fresh download, `Ok(None)` if the archive
    /// already contained the id (so the DB fell out of sync with the archive —
    /// the caller should mark the item done without a file path).
    pub fn download_audio(
        &self,
        video_id: &str,
        dest_dir: &Path,
        archive_path: &Path,
    ) -> std::result::Result<Option<DownloadResult>, YtDlpError> {
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
        // with the final on-disk path. If the archive already contains this id,
        // yt-dlp exits 0 with no filepath and logs the archive-skip on stderr.
        match extract_filepath(&out.stdout) {
            Some(file_path) => {
                let title = file_path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| video_id.to_string());
                Ok(Some(DownloadResult { file_path, title }))
            }
            None if looks_like_archive_hit(&out.stderr) => Ok(None),
            None => Err(YtDlpError {
                message: format!("yt-dlp reported success but emitted no filepath for {video_id}"),
                stderr: out.stderr.clone(),
                exit_code: out.exit_status.code(),
                looks_like_extractor: false,
                looks_like_auth: false,
                timed_out: false,
            }),
        }
    }

    /// Downloads one video id as MKV into `dest_dir`.
    /// Native codecs from YouTube are kept (typically VP9 + Opus or AV1 + Opus)
    /// to avoid Pi-side transcoding — container is MKV via --merge-output-format.
    /// Optional `quality_cap` like "1080p" or "720p" limits height.
    pub fn download_video(
        &self,
        video_id: &str,
        dest_dir: &Path,
        archive_path: &Path,
        quality_cap: Option<&str>,
    ) -> std::result::Result<Option<DownloadResult>, YtDlpError> {
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
        let format_spec = build_video_format(quality_cap);

        let mut cmd = Command::new(&self.binary);
        cmd.arg("--cookies")
            .arg(&self.cookies)
            .arg("--download-archive")
            .arg(archive_path)
            .arg("--no-overwrites")
            .arg("--no-progress")
            .arg("--no-warnings")
            .arg("--retries")
            .arg("1")
            .arg("--fragment-retries")
            .arg("3")
            .arg("--limit-rate")
            .arg(&self.cfg.rate_limit)
            .arg("--sleep-interval")
            .arg(self.cfg.sleep_interval_sec.to_string())
            .arg("--max-sleep-interval")
            .arg(self.cfg.max_sleep_interval_sec.to_string())
            .arg("-f")
            .arg(&format_spec)
            .arg("--merge-output-format")
            .arg("mkv")
            .arg("--embed-metadata")
            .arg("--embed-thumbnail")
            .arg("--embed-chapters")
            .arg("--embed-subs")
            .arg("--sub-langs")
            .arg("en,de")
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
                message: format!("video download failed for {video_id}"),
                exit_code: out.exit_status.code(),
                looks_like_extractor: YtDlpUpdater::looks_like_extraction_failure(&out.stderr),
                looks_like_auth: YtDlpUpdater::looks_like_auth_failure(&out.stderr),
                stderr: out.stderr,
                timed_out: false,
            });
        }

        match extract_filepath(&out.stdout) {
            Some(file_path) => {
                let title = file_path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| video_id.to_string());
                Ok(Some(DownloadResult { file_path, title }))
            }
            None if looks_like_archive_hit(&out.stderr) => Ok(None),
            None => Err(YtDlpError {
                message: format!("yt-dlp reported success but emitted no filepath for {video_id}"),
                stderr: out.stderr.clone(),
                exit_code: out.exit_status.code(),
                looks_like_extractor: false,
                looks_like_auth: false,
                timed_out: false,
            }),
        }
    }
}

fn extract_filepath(stdout: &str) -> Option<PathBuf> {
    stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| PathBuf::from(l.trim()))
}

/// yt-dlp prints this (roughly) on stderr when `--download-archive` already
/// lists the video id. Match on a substring that is stable across versions.
fn looks_like_archive_hit(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    lower.contains("has already been recorded in the archive")
        || lower.contains("already in archive")
}

/// Builds the yt-dlp `-f` format expression for video downloads. Applies a
/// height cap derived from a string like "1080p" when present.
fn build_video_format(quality_cap: Option<&str>) -> String {
    let height = quality_cap
        .and_then(parse_quality_height)
        .filter(|h| *h >= 144 && *h <= 4320);
    match height {
        Some(h) => format!("bv*[height<={h}]+ba/b[height<={h}]"),
        None => "bv*+ba/b".to_string(),
    }
}

fn parse_quality_height(q: &str) -> Option<u32> {
    let trimmed = q.trim().trim_end_matches('p').trim_end_matches('P');
    trimmed.parse::<u32>().ok()
}

struct InvocationOutput {
    exit_status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

/// Per-stream read cap. 1 MiB is generous for yt-dlp's regular chatter but
/// stops a debug-log blowup from eating the service's 200M RAM cap.
const STREAM_CAP_BYTES: usize = 1_048_576;

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

    // Drain stdout and stderr concurrently. Without this, a child that writes
    // more than the 64 KiB OS pipe buffer will block on write, and wait_timeout
    // will appear to hang for the full timeout even though the process is fine.
    let stdout_pipe = child.stdout.take().expect("stdout was piped");
    let stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stdout_handle = std::thread::spawn(move || drain_capped(stdout_pipe, STREAM_CAP_BYTES));
    let stderr_handle = std::thread::spawn(move || drain_capped(stderr_pipe, STREAM_CAP_BYTES));

    let (status, timeout_err) = match child.wait_timeout(timeout) {
        Ok(Some(s)) => (Some(s), None),
        Ok(None) => {
            warn!("yt-dlp exceeded {}s timeout; killing", timeout.as_secs());
            let _ = child.kill();
            let _ = child.wait();
            (
                None,
                Some((
                    format!("yt-dlp timed out after {}s", timeout.as_secs()),
                    true,
                )),
            )
        }
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            (None, Some((format!("wait_timeout error: {e}"), false)))
        }
    };

    let (stdout_bytes, stdout_trunc) = stdout_handle.join().unwrap_or_default();
    let (stderr_bytes, stderr_trunc) = stderr_handle.join().unwrap_or_default();
    let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let mut stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();
    if stdout_trunc || stderr_trunc {
        stderr.push_str("\n[stderr/stdout truncated at 1 MiB]");
    }

    if let Some((msg, timed_out)) = timeout_err {
        return Err(YtDlpError {
            message: msg,
            stderr,
            exit_code: None,
            looks_like_extractor: false,
            looks_like_auth: false,
            timed_out,
        });
    }
    Ok(InvocationOutput {
        exit_status: status.expect("status is Some when no timeout_err"),
        stdout,
        stderr,
    })
}

/// Reads until EOF, keeping at most `cap` bytes. Continues to drain past the
/// cap (discarding) so the child process never blocks writing to a full pipe.
fn drain_capped<R: std::io::Read>(mut r: R, cap: usize) -> (Vec<u8>, bool) {
    let mut buf: Vec<u8> = Vec::with_capacity(std::cmp::min(cap, 16 * 1024));
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let room = cap.saturating_sub(buf.len());
                if room >= n {
                    buf.extend_from_slice(&chunk[..n]);
                } else {
                    if room > 0 {
                        buf.extend_from_slice(&chunk[..room]);
                    }
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (buf, truncated)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_format_without_cap() {
        assert_eq!(build_video_format(None), "bv*+ba/b");
    }

    #[test]
    fn video_format_with_quality_cap() {
        assert_eq!(
            build_video_format(Some("1080p")),
            "bv*[height<=1080]+ba/b[height<=1080]"
        );
        assert_eq!(
            build_video_format(Some("720P")),
            "bv*[height<=720]+ba/b[height<=720]"
        );
        assert_eq!(
            build_video_format(Some("480")),
            "bv*[height<=480]+ba/b[height<=480]"
        );
    }

    #[test]
    fn nonsense_quality_falls_back_to_uncapped() {
        assert_eq!(build_video_format(Some("hd")), "bv*+ba/b");
        assert_eq!(build_video_format(Some("")), "bv*+ba/b");
        assert_eq!(build_video_format(Some("9999p")), "bv*+ba/b"); // outside valid range
    }

    #[test]
    fn detects_archive_hit() {
        assert!(looks_like_archive_hit(
            "[download] abc123 has already been recorded in the archive"
        ));
        assert!(looks_like_archive_hit("[youtube] Already in archive: XYZ"));
        assert!(!looks_like_archive_hit(
            "ERROR: unable to download video data"
        ));
        assert!(!looks_like_archive_hit(""));
    }
}
