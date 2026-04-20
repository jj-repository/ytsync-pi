// A handful of struct fields (YtDlpError diagnostics, UpdateOutcome stdout,
// config.bitrate) exist as stable public surface for subprocess wrappers but
// are not yet read back at the call sites. Keep them so future logging /
// telemetry work doesn't have to re-break the API.
#![allow(dead_code)]

mod config;
mod db;
mod lock;
mod musicbrainz;
mod ntfy;
mod preflight;
mod shutdown;
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
    /// Send a test ntfy notification to confirm alert delivery
    TestNtfy,
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
        Command::TestNtfy => cmd_test_ntfy(&cfg_path),
    }
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "unknown-host".to_string())
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

    let notifier = ntfy::Notifier::from_config(cfg.ntfy.clone());
    let host = hostname();
    let shutdown =
        shutdown::ShutdownFlag::install().context("install signal handlers for SIGTERM/SIGINT")?;

    let updater = ytdlp_updater::YtDlpUpdater::new(cfg.yt_dlp.clone());
    if let Err(e) = updater.ensure_installed() {
        let msg = format!("yt-dlp not installed: {e}");
        database.finish_run(run_id, 0, 0, Some(&msg), false)?;
        if let Some(n) = notifier.as_ref() {
            let stats = sync::SyncStats {
                ok: 0,
                failed: 1,
                skipped_sources: 0,
                tagged: 0,
                tag_no_match: 0,
                tag_skipped: 0,
                cookies_suspicious: false,
            };
            n.report_run(run_id, &stats, "<not installed>", &host);
        }
        return Err(e);
    }

    let _update_outcome = updater.ensure_fresh();
    let version = updater
        .version()
        .unwrap_or_else(|e| format!("<version probe failed: {e}>"));
    info!("yt-dlp version: {version}");

    match database.prune_failures(90) {
        Ok(0) => {}
        Ok(n) => info!("pruned {n} failure row(s) older than 90 days"),
        Err(e) => warn!("prune_failures: {e}"),
    }

    let stats = tracing::info_span!("run", id = run_id)
        .in_scope(|| sync::run_sync(&cfg, &database, &updater, &shutdown));
    let aborted = shutdown.is_set();

    let notes = format!(
        "yt-dlp={version}; ok={} fail={} skipped_sources={} tagged={} tag_no_match={} tag_skipped={} cookies_suspicious={} aborted={}",
        stats.ok,
        stats.failed,
        stats.skipped_sources,
        stats.tagged,
        stats.tag_no_match,
        stats.tag_skipped,
        stats.cookies_suspicious,
        aborted,
    );
    if aborted {
        warn!("run {run_id} interrupted by SIGTERM/SIGINT; finalizing partial stats");
    }
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
    if let Some(n) = notifier.as_ref() {
        n.report_run(run_id, &stats, &version, &host);
    }
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

    if cfg.cookies_path.exists() {
        if let Ok(meta) = std::fs::metadata(&cfg.cookies_path) {
            if let Ok(modified) = meta.modified() {
                let age = std::time::SystemTime::now()
                    .duration_since(modified)
                    .unwrap_or_default()
                    .as_secs()
                    / 86_400;
                println!(
                    "cookies file:       {} ({} days old)",
                    cfg.cookies_path.display(),
                    age
                );
            }
        }
    } else {
        println!(
            "cookies file:       MISSING at {}",
            cfg.cookies_path.display()
        );
    }

    let bin = &cfg.yt_dlp.binary_path;
    if bin.exists() {
        let age_days = std::fs::metadata(bin)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| std::time::SystemTime::now().duration_since(t).ok())
            .map(|d| d.as_secs() / 86_400);
        match age_days {
            Some(days) => println!("yt-dlp binary:      {} ({days} days old)", bin.display()),
            None => println!("yt-dlp binary:      {}", bin.display()),
        }
    } else {
        println!("yt-dlp binary:      MISSING at {}", bin.display());
    }

    println!(
        "ntfy alerts:        {}",
        match &cfg.ntfy {
            Some(n) if n.enabled => format!("enabled → {}/{}", n.server, n.topic),
            Some(_) => "configured but disabled".to_string(),
            None => "not configured".to_string(),
        }
    );
    Ok(())
}

fn cmd_test_ntfy(cfg_path: &std::path::Path) -> Result<()> {
    let cfg = config::Config::load(cfg_path)?;
    let notifier = ntfy::Notifier::from_config(cfg.ntfy.clone())
        .ok_or_else(|| anyhow::anyhow!("ntfy not configured or disabled in {cfg_path:?}"))?;
    let host = hostname();
    if notifier.send_test(&host) {
        println!("ntfy test notification sent");
        Ok(())
    } else {
        anyhow::bail!("ntfy send failed — see log for details")
    }
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
