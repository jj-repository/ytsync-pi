use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::flag;

/// Wires SIGTERM / SIGINT to an atomic flag the sync loop polls between items.
/// systemd sends SIGTERM first on stop; we have `TimeoutStopSec` to unwind
/// cleanly (finalize the run row, drop the DB handle, release the lock) before
/// SIGKILL arrives.
#[derive(Clone)]
pub struct ShutdownFlag(Arc<AtomicBool>);

impl ShutdownFlag {
    pub fn install() -> Result<Self> {
        let flag_inner = Arc::new(AtomicBool::new(false));
        flag::register(SIGTERM, Arc::clone(&flag_inner))?;
        flag::register(SIGINT, Arc::clone(&flag_inner))?;
        Ok(Self(flag_inner))
    }

    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}
