use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};
use tracing::{info, warn};
use wait_timeout::ChildExt;

use crate::config::YtDlpConfig;

/// Known stderr substrings that suggest yt-dlp's extractors are out of date.
/// Kept lowercase; caller lower-cases stderr before matching.
const EXTRACTION_ERROR_SIGNATURES: &[&str] = &[
    "unable to extract",
    "signature extraction failed",
    "precondition check failed",
    "http error 403",
    "youtube said: the following content",
    "unable to download webpage",
    "requested format is not available",
    "no longer available",
];

/// Known stderr substrings that suggest the cookies file is stale — i.e.
/// YouTube rejected it for auth, not because an extractor broke. These drive
/// a separate notification path so the user knows to re-export cookies rather
/// than wait for yt-dlp to update itself.
const AUTH_FAILURE_SIGNATURES: &[&str] = &[
    "sign in to confirm",
    "sign in, to confirm",
    "not a bot",
    "login required",
    "please log in",
    "http error 401",
    "private video",
    "members-only content",
    "this video is available to this channel's members",
    "the uploader has not made this video available",
    "cookies provided do not match",
    "cookies are no longer valid",
    "your cookies are expired",
    "requested content is not available, rechecking",
];

pub struct YtDlpUpdater {
    cfg: YtDlpConfig,
}

pub struct UpdateOutcome {
    pub attempted: bool,
    pub succeeded: bool,
    pub stdout: String,
    pub stderr: String,
    pub new_version: Option<String>,
}

impl YtDlpUpdater {
    pub fn new(cfg: YtDlpConfig) -> Self {
        Self { cfg }
    }

    pub fn binary(&self) -> &Path {
        &self.cfg.binary_path
    }

    pub fn ensure_installed(&self) -> Result<()> {
        if !self.cfg.binary_path.exists() {
            return Err(anyhow!(
                "yt-dlp not found at {}. Install it first (e.g. `curl -L https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp -o {} && chmod +x {}`)",
                self.cfg.binary_path.display(),
                self.cfg.binary_path.display(),
                self.cfg.binary_path.display(),
            ));
        }
        Ok(())
    }

    /// Run `yt-dlp --version`, returning the trimmed first line.
    pub fn version(&self) -> Result<String> {
        let out = Command::new(&self.cfg.binary_path)
            .arg("--version")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| format!("exec {} --version", self.cfg.binary_path.display()))?;
        if !out.status.success() {
            return Err(anyhow!(
                "yt-dlp --version exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Returns Some(age) if the binary exists and we could stat it.
    pub fn binary_age(&self) -> Option<Duration> {
        let meta = std::fs::metadata(&self.cfg.binary_path).ok()?;
        let mtime = meta.modified().ok()?;
        SystemTime::now().duration_since(mtime).ok()
    }

    /// If auto-update is on and the binary is older than the configured
    /// threshold, run an update. Never returns an error for update failures —
    /// they are logged and surfaced via the returned outcome.
    pub fn ensure_fresh(&self) -> UpdateOutcome {
        if !self.cfg.auto_update {
            return UpdateOutcome::skipped();
        }
        let Some(age) = self.binary_age() else {
            warn!(
                "cannot stat yt-dlp at {}; skipping age-based update",
                self.cfg.binary_path.display()
            );
            return UpdateOutcome::skipped();
        };
        let threshold = Duration::from_secs(self.cfg.update_if_older_than_days * 86_400);
        if age < threshold {
            return UpdateOutcome::skipped();
        }
        info!(
            "yt-dlp binary is {} days old (threshold {} days); updating via {} channel",
            age.as_secs() / 86_400,
            self.cfg.update_if_older_than_days,
            self.cfg.channel.as_str()
        );
        self.update_now()
    }

    /// Force an update regardless of age. Non-fatal on failure.
    pub fn update_now(&self) -> UpdateOutcome {
        let mut cmd = Command::new(&self.cfg.binary_path);
        cmd.arg("-U")
            .arg("--update-to")
            .arg(self.cfg.channel.as_str())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                warn!("failed to spawn yt-dlp -U: {e}");
                return UpdateOutcome::failed(String::new(), e.to_string());
            }
        };

        // Drain pipes on dedicated threads before waiting, so the child can never
        // block on a full 64 KiB OS pipe buffer (same pattern as run_with_timeout
        // in ytdlp.rs). Without this, a chatty -U run deadlocks the wait.
        let stdout_pipe = child.stdout.take().expect("stdout was piped");
        let stderr_pipe = child.stderr.take().expect("stderr was piped");
        let stdout_handle = std::thread::spawn(move || {
            crate::ytdlp::drain_capped(stdout_pipe, crate::ytdlp::STREAM_CAP_BYTES)
        });
        let stderr_handle = std::thread::spawn(move || {
            crate::ytdlp::drain_capped(stderr_pipe, crate::ytdlp::STREAM_CAP_BYTES)
        });

        let timeout = Duration::from_secs(self.cfg.update_timeout_sec);
        let status = match child.wait_timeout(timeout) {
            Ok(Some(s)) => s,
            Ok(None) => {
                warn!(
                    "yt-dlp -U exceeded {}s timeout; killing",
                    self.cfg.update_timeout_sec
                );
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_handle.join();
                let _ = stderr_handle.join();
                return UpdateOutcome::failed(String::new(), "update timed out".to_string());
            }
            Err(e) => {
                warn!("yt-dlp -U wait error: {e}");
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_handle.join();
                let _ = stderr_handle.join();
                return UpdateOutcome::failed(String::new(), e.to_string());
            }
        };

        let (stdout_bytes, _) = stdout_handle.join().unwrap_or_default();
        let (stderr_bytes, _) = stderr_handle.join().unwrap_or_default();
        let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
        let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();

        if !status.success() {
            warn!(
                "yt-dlp -U failed (exit {:?}). stderr: {}",
                status.code(),
                stderr.trim()
            );
            return UpdateOutcome::failed(stdout, stderr);
        }

        let new_version = self.version().ok();
        info!(
            "yt-dlp update OK (channel {}); version now {}",
            self.cfg.channel.as_str(),
            new_version.as_deref().unwrap_or("<unknown>")
        );

        UpdateOutcome {
            attempted: true,
            succeeded: true,
            stdout,
            stderr,
            new_version,
        }
    }

    /// Returns true if `stderr_text` looks like yt-dlp's extractor is out of date.
    pub fn looks_like_extraction_failure(stderr_text: &str) -> bool {
        let lower = stderr_text.to_ascii_lowercase();
        EXTRACTION_ERROR_SIGNATURES
            .iter()
            .any(|sig| lower.contains(sig))
    }

    /// Returns true if `stderr_text` looks like cookies have expired or been
    /// rejected by YouTube. Distinct from extractor-failure — the fix is
    /// re-exporting cookies, not bumping yt-dlp.
    pub fn looks_like_auth_failure(stderr_text: &str) -> bool {
        let lower = stderr_text.to_ascii_lowercase();
        AUTH_FAILURE_SIGNATURES
            .iter()
            .any(|sig| lower.contains(sig))
    }

    pub fn binary_path(&self) -> PathBuf {
        self.cfg.binary_path.clone()
    }
}

impl UpdateOutcome {
    pub fn skipped() -> Self {
        Self {
            attempted: false,
            succeeded: false,
            stdout: String::new(),
            stderr: String::new(),
            new_version: None,
        }
    }

    pub fn failed(stdout: String, stderr: String) -> Self {
        Self {
            attempted: true,
            succeeded: false,
            stdout,
            stderr,
            new_version: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_known_extraction_failures() {
        for sig in [
            "ERROR: Unable to extract video data",
            "ERROR: Signature extraction failed",
            "HTTP Error 403: Forbidden",
            "ERROR: Precondition check failed",
        ] {
            assert!(
                YtDlpUpdater::looks_like_extraction_failure(sig),
                "signature should match: {sig}"
            );
        }
    }

    #[test]
    fn benign_errors_do_not_match() {
        assert!(!YtDlpUpdater::looks_like_extraction_failure(
            "network timeout"
        ));
        assert!(!YtDlpUpdater::looks_like_extraction_failure(
            "disk full: no space left on device"
        ));
    }

    #[test]
    fn detects_known_auth_failures() {
        for sig in [
            "ERROR: Sign in to confirm you're not a bot",
            "ERROR: Private video. Sign in if you've been granted access",
            "ERROR: members-only content",
            "HTTP Error 401: Unauthorized",
            "ERROR: The uploader has not made this video available",
            "requested content is not available, rechecking",
        ] {
            assert!(
                YtDlpUpdater::looks_like_auth_failure(sig),
                "auth signature should match: {sig}"
            );
        }
    }

    #[test]
    fn auth_and_extraction_classifiers_are_disjoint_on_clear_cases() {
        let auth = "ERROR: Sign in to confirm you're not a bot";
        let extract = "ERROR: Unable to extract player response";
        assert!(YtDlpUpdater::looks_like_auth_failure(auth));
        assert!(!YtDlpUpdater::looks_like_auth_failure(extract));
        assert!(YtDlpUpdater::looks_like_extraction_failure(extract));
        assert!(!YtDlpUpdater::looks_like_extraction_failure(auth));
    }
}
