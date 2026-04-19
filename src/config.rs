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

    pub sources: Vec<Source>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct NtfyConfig {
    pub server: String,
    pub topic: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MusicBrainzConfig {
    pub acoustid_api_key: String,
    #[serde(default = "default_mb_enabled")]
    pub enabled: bool,
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
