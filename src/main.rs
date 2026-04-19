// Phase 1 scaffold: many DB/config surfaces exist for the phase-2 sync pipeline.
// Remove this allow once phase 2 wires them up.
#![allow(dead_code)]

mod config;
mod db;
mod lock;

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
            return Ok(());
        }
    };

    let database = db::Db::open(&cfg.db_path)?;
    let run_id = database.start_run()?;
    info!(
        "run {} started; {} sources configured",
        run_id,
        cfg.sources.len()
    );

    // Phase 2 will plug the sync pipeline in here.
    let notes = "scaffold run — sync pipeline not yet implemented";
    database.finish_run(run_id, 0, 0, Some(notes))?;
    info!("run {} finished (no-op scaffold)", run_id);
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
            if let Some(n) = s.notes {
                println!("  notes:            {n}");
            }
        }
        None => println!("no runs recorded yet"),
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
    info!("(live YouTube probe will be added in phase 2)");
    Ok(())
}

fn cmd_show_config(cfg_path: &std::path::Path) -> Result<()> {
    let cfg = config::Config::load(cfg_path)
        .with_context(|| format!("loading {}", cfg_path.display()))?;
    println!("{cfg:#?}");
    Ok(())
}
