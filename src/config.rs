use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub cookies_path: PathBuf,
    pub output_audio_dir: PathBuf,
    pub output_video_dir: PathBuf,
    pub db_path: PathBuf,
    pub lock_path: PathBuf,

    #[serde(default = "default_bitrate")]
    pub bitrate: String,
    #[serde(default = "default_rate_limit")]
    pub rate_limit: String,
    #[serde(default = "default_retries")]
    pub retries: u32,
    #[serde(default = "default_retry_backoff")]
    pub retry_backoff_sec: u64,
    #[serde(default = "default_sleep_min")]
    pub sleep_interval_sec: u32,
    #[serde(default = "default_sleep_max")]
    pub max_sleep_interval_sec: u32,
    #[serde(default = "default_timeout")]
    pub per_item_timeout_sec: u64,
    #[serde(default = "default_min_free_gb")]
    pub min_free_disk_gb: u64,

    #[serde(default)]
    pub ntfy: Option<NtfyConfig>,
    #[serde(default)]
    pub musicbrainz: Option<MusicBrainzConfig>,
    #[serde(default)]
    pub yt_dlp: YtDlpConfig,

    pub sources: Vec<Source>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct YtDlpConfig {
    #[serde(default = "default_ytdlp_binary")]
    pub binary_path: PathBuf,
    #[serde(default = "default_true")]
    pub auto_update: bool,
    #[serde(default = "default_update_age_days")]
    pub update_if_older_than_days: u64,
    #[serde(default = "default_channel")]
    pub channel: YtDlpChannel,
    #[serde(default = "default_true")]
    pub update_on_extract_error: bool,
    #[serde(default = "default_update_timeout")]
    pub update_timeout_sec: u64,
}

impl Default for YtDlpConfig {
    fn default() -> Self {
        Self {
            binary_path: default_ytdlp_binary(),
            auto_update: true,
            update_if_older_than_days: default_update_age_days(),
            channel: default_channel(),
            update_on_extract_error: true,
            update_timeout_sec: default_update_timeout(),
        }
    }
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum YtDlpChannel {
    Stable,
    Nightly,
    Master,
}

impl YtDlpChannel {
    pub fn as_str(self) -> &'static str {
        match self {
            YtDlpChannel::Stable => "stable",
            YtDlpChannel::Nightly => "nightly",
            YtDlpChannel::Master => "master",
        }
    }
}

#[derive(Deserialize, Clone)]
pub struct NtfyConfig {
    pub server: String,
    pub topic: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Token for authenticated ntfy topics. Leave empty for public topics.
    #[serde(default)]
    pub token: Option<String>,
}

impl std::fmt::Debug for NtfyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NtfyConfig")
            .field("server", &self.server)
            .field("topic", &self.topic)
            .field("enabled", &self.enabled)
            .field("token", &redact(self.token.as_deref()))
            .finish()
    }
}

#[derive(Deserialize, Clone)]
pub struct MusicBrainzConfig {
    pub acoustid_api_key: String,
    #[serde(default = "default_mb_enabled")]
    pub enabled: bool,
}

impl std::fmt::Debug for MusicBrainzConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MusicBrainzConfig")
            .field("acoustid_api_key", &redact(Some(&self.acoustid_api_key)))
            .field("enabled", &self.enabled)
            .finish()
    }
}

fn redact(secret: Option<&str>) -> &'static str {
    match secret {
        Some(s) if !s.is_empty() => "***set***",
        _ => "<unset>",
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct Source {
    pub name: String,
    pub url: String,
    pub mode: SourceMode,
    #[serde(default)]
    pub quality: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SourceMode {
    Audio,
    Video,
}

fn default_bitrate() -> String {
    "320k".to_string()
}
fn default_rate_limit() -> String {
    "2M".to_string()
}
fn default_retries() -> u32 {
    3
}
fn default_retry_backoff() -> u64 {
    60
}
fn default_sleep_min() -> u32 {
    5
}
fn default_sleep_max() -> u32 {
    15
}
fn default_timeout() -> u64 {
    1800
}
fn default_min_free_gb() -> u64 {
    2
}
fn default_mb_enabled() -> bool {
    true
}
fn default_true() -> bool {
    true
}
fn default_ytdlp_binary() -> PathBuf {
    PathBuf::from("~/.local/bin/yt-dlp")
}
fn default_update_age_days() -> u64 {
    3
}
fn default_channel() -> YtDlpChannel {
    YtDlpChannel::Nightly
}
fn default_update_timeout() -> u64 {
    120
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&raw).with_context(|| format!("parse config {}", path.display()))?;
        cfg.expand_paths()?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn expand_paths(&mut self) -> Result<()> {
        self.cookies_path = expand(&self.cookies_path)?;
        self.output_audio_dir = expand(&self.output_audio_dir)?;
        self.output_video_dir = expand(&self.output_video_dir)?;
        self.db_path = expand(&self.db_path)?;
        self.lock_path = expand(&self.lock_path)?;
        self.yt_dlp.binary_path = expand(&self.yt_dlp.binary_path)?;
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.sources.is_empty() {
            anyhow::bail!("config has no sources");
        }
        for s in &self.sources {
            if s.name.trim().is_empty() {
                anyhow::bail!("source has empty name");
            }
            if s.url.trim().is_empty() {
                anyhow::bail!("source {} has empty url", s.name);
            }
        }
        Ok(())
    }
}

fn expand(p: &Path) -> Result<PathBuf> {
    let s = p.to_string_lossy();
    let expanded = shellexpand::full(&s).with_context(|| format!("expand path {s}"))?;
    Ok(PathBuf::from(expanded.into_owned()))
}

pub fn default_config_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "ytsync-pi").context("resolve config dir")?;
    Ok(dirs.config_dir().join("config.toml"))
}
