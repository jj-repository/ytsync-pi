// Phase 3: config/DB/yt-dlp error surfaces exist for MusicBrainz (phase 4),
// video mode (phase 5), and ntfy (phase 7). Remove this allow as those
// phases wire the fields in.
#![allow(dead_code)]

mod config;
mod db;
mod lock;
mod musicbrainz;
mod preflight;
mod sync;
mod ytdlp;
mod ytdlp_updater;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "ytsync-pi",
    version,
    about = "Sync YouTube playlists/likes to NAS"
)]
struct Cli {
    /// Path to config file (defaults to XDG config dir)
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run a sync cycle (default behavior when invoked by systemd timer)
    Run,
    /// Show last run summary and open failures
    Status,
    /// Validate config and probe cookies by listing one playlist
    TestCookies,
    /// Print the resolved config and exit
    ShowConfig,
    /// Force an immediate yt-dlp self-update regardless of binary age
    UpdateYtdlp,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let cfg_path = match cli.config {
        Some(p) => p,
        None => config::default_config_path()?,
    };

    match cli.command {
        Command::Run => cmd_run(&cfg_path),
        Command::Status => cmd_status(&cfg_path),
        Command::TestCookies => cmd_test_cookies(&cfg_path),
        Command::ShowConfig => cmd_show_config(&cfg_path),
        Command::UpdateYtdlp => cmd_update_ytdlp(&cfg_path),
    }
}

fn cmd_run(cfg_path: &std::path::Path) -> Result<()> {
    let cfg = config::Config::load(cfg_path)?;
    let _lock = match lock::RunLock::acquire(&cfg.lock_path)? {
        Some(l) => l,
        None => {
            warn!(
                "another ytsync-pi run holds the lock at {}; exiting",
                cfg.lock_path.display()
            );
            std::process::exit(2);
        }
    };

    let report = preflight::run(&cfg)?;
    if let Some(days) = report.cookies_age_days {
        info!("cookies file age: {days} days");
    }

    let database = db::Db::open(&cfg.db_path)?;
    let run_id = database.start_run()?;
    info!(
        "run {} started; {} sources configured",
        run_id,
        cfg.sources.len()
    );

    let updater = ytdlp_updater::YtDlpUpdater::new(cfg.yt_dlp.clone());
    if let Err(e) = updater.ensure_installed() {
        database.finish_run(
            run_id,
            0,
            0,
            Some(&format!("yt-dlp not installed: {e}")),
            false,
        )?;
        return Err(e);
    }

    let _update_outcome = updater.ensure_fresh();
    let version = updater
        .version()
        .unwrap_or_else(|e| format!("<version probe failed: {e}>"));
    info!("yt-dlp version: {version}");

    let stats = sync::run_sync(&cfg, &database, &updater);

    let notes = format!(
        "yt-dlp={version}; ok={} fail={} skipped_sources={} tagged={} tag_no_match={} tag_skipped={} cookies_suspicious={}",
        stats.ok,
        stats.failed,
        stats.skipped_sources,
        stats.tagged,
        stats.tag_no_match,
        stats.tag_skipped,
        stats.cookies_suspicious,
    );
    database.finish_run(
        run_id,
        stats.ok,
        stats.failed,
        Some(&notes),
        stats.cookies_suspicious,
    )?;
    if stats.cookies_suspicious {
        warn!(
            "run {run_id} finished with cookies_suspicious=TRUE — re-export your browser cookies to {}",
            cfg.cookies_path.display()
        );
    }
    info!(
        "run {run_id} finished: ok={} fail={} skipped_sources={} tagged={} cookies_suspicious={}",
        stats.ok, stats.failed, stats.skipped_sources, stats.tagged, stats.cookies_suspicious,
    );
    Ok(())
}

fn cmd_status(cfg_path: &std::path::Path) -> Result<()> {
    let cfg = config::Config::load(cfg_path)?;
    let database = db::Db::open(&cfg.db_path)?;
    match database.last_run_summary()? {
        Some(s) => {
            println!("last run id:        {}", s.id);
            println!("  started_at:       {}", s.started_at);
            println!("  finished_at:      {:?}", s.finished_at);
            println!("  ok / fail:        {} / {}", s.ok_count, s.fail_count);
            if s.cookies_suspicious {
                println!("  ⚠ cookies:         LIKELY EXPIRED — re-export from browser");
            }
            if let Some(n) = s.notes {
                println!("  notes:            {n}");
            }
        }
        None => println!("no runs recorded yet"),
    }
    if let Some(run_id) = database.last_cookies_warning_run()? {
        println!("last cookies warning at run #{run_id}");
    }
    println!("open failures:      {}", database.failure_count()?);
    Ok(())
}

fn cmd_test_cookies(cfg_path: &std::path::Path) -> Result<()> {
    let cfg = config::Config::load(cfg_path)?;
    if !cfg.cookies_path.exists() {
        anyhow::bail!("cookies file not found at {}", cfg.cookies_path.display());
    }
    info!("cookies file present at {}", cfg.cookies_path.display());

    let updater = ytdlp_updater::YtDlpUpdater::new(cfg.yt_dlp.clone());
    updater.ensure_installed()?;
    let yt = ytdlp::YtDlp::new(updater.binary_path(), &cfg);

    let first = cfg
        .sources
        .first()
        .ok_or_else(|| anyhow::anyhow!("no sources configured"))?;
    info!("probing with first source {:?} ({})", first.name, first.url);
    match yt.list_playlist(&first.url) {
        Ok(entries) => {
            println!(
                "cookies OK — listed {} entries from {:?}",
                entries.len(),
                first.name
            );
            if let Some(sample) = entries.first() {
                println!("  first entry: {} | {}", sample.id, sample.title);
            }
            Ok(())
        }
        Err(e) => Err(anyhow::anyhow!(
            "cookies probe failed: {}; stderr: {}",
            e.message,
            e.stderr.trim()
        )),
    }
}

fn cmd_show_config(cfg_path: &std::path::Path) -> Result<()> {
    let cfg = config::Config::load(cfg_path)
        .with_context(|| format!("loading {}", cfg_path.display()))?;
    println!("{cfg:#?}");
    Ok(())
}

fn cmd_update_ytdlp(cfg_path: &std::path::Path) -> Result<()> {
    let cfg = config::Config::load(cfg_path)?;
    let updater = ytdlp_updater::YtDlpUpdater::new(cfg.yt_dlp.clone());
    updater.ensure_installed()?;
    let before = updater
        .version()
        .unwrap_or_else(|_| "<unknown>".to_string());
    info!("yt-dlp before: {before}");
    let outcome = updater.update_now();
    if !outcome.succeeded {
        anyhow::bail!("yt-dlp update failed; stderr: {}", outcome.stderr.trim());
    }
    let after = outcome
        .new_version
        .clone()
        .unwrap_or_else(|| "<unknown>".to_string());
    println!("yt-dlp: {before} -> {after}");
    Ok(())
}
