use std::time::Duration;
use tracing::{debug, warn};

use crate::config::NtfyConfig;
use crate::sync::SyncStats;

/// Thin ntfy.sh client. Every send is best-effort: a network failure here must
/// never escalate — the run itself already succeeded or failed on its own merits.
pub struct Notifier {
    cfg: NtfyConfig,
    agent: ureq::Agent,
}

impl Notifier {
    pub fn from_config(cfg: Option<NtfyConfig>) -> Option<Self> {
        let cfg = cfg?;
        if !cfg.enabled {
            return None;
        }
        if cfg.server.trim().is_empty() || cfg.topic.trim().is_empty() {
            warn!("ntfy configured but server or topic is empty; alerts disabled");
            return None;
        }
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout_read(Duration::from_secs(10))
            .timeout_write(Duration::from_secs(10))
            .build();
        Some(Self { cfg, agent })
    }

    /// Send a one-off notification. Returns false on any error; errors are logged
    /// at warn level, never propagated.
    fn send(&self, title: &str, body: &str, priority: u8, tags: &[&str]) -> bool {
        let url = format!(
            "{}/{}",
            self.cfg.server.trim_end_matches('/'),
            self.cfg.topic
        );
        let tags_joined = tags.join(",");
        let prio = priority.clamp(1, 5).to_string();
        let mut req = self
            .agent
            .post(&url)
            .set("Title", title)
            .set("Priority", &prio)
            .set("Tags", &tags_joined)
            .set("Content-Type", "text/plain; charset=utf-8");
        if let Some(token) = self.cfg.token.as_deref() {
            if !token.is_empty() {
                req = req.set("Authorization", &format!("Bearer {token}"));
            }
        }
        match req.send_string(body) {
            Ok(_) => {
                debug!("ntfy sent: {title}");
                true
            }
            Err(e) => {
                warn!("ntfy send failed ({url}): {e}");
                false
            }
        }
    }

    /// Decide what (if anything) to send for a completed run. Two independent
    /// signals: download failures and cookies_suspicious. Cookies get a dedicated
    /// higher-priority alert because an expired cookies file produces zero
    /// downloads and would otherwise look like "nothing new today".
    pub fn report_run(&self, run_id: i64, stats: &SyncStats, version: &str, host: &str) {
        if stats.cookies_suspicious {
            let body = format!(
                "ytsync-pi run #{run_id} on {host} saw auth failures.\n\
                 Cookies at the configured path are likely expired.\n\
                 Re-export from your browser and rerun.\n\
                 yt-dlp={version} ok={} fail={}",
                stats.ok, stats.failed,
            );
            self.send(
                "ytsync-pi: cookies likely expired",
                &body,
                4,
                &["warning", "cookie", "lock"],
            );
        }

        if stats.failed > 0 {
            let body = format!(
                "ytsync-pi run #{run_id} on {host} finished with failures.\n\
                 ok={} fail={} skipped_sources={}\n\
                 tagged={} tag_no_match={} tag_skipped={}\n\
                 yt-dlp={version}",
                stats.ok,
                stats.failed,
                stats.skipped_sources,
                stats.tagged,
                stats.tag_no_match,
                stats.tag_skipped,
            );
            self.send(
                &format!("ytsync-pi: {} failure(s)", stats.failed),
                &body,
                3,
                &["x", "film_strip"],
            );
        }
    }

    /// Manual probe used by the canary to confirm alerts still reach a device.
    pub fn send_test(&self, host: &str) -> bool {
        self.send(
            "ytsync-pi: canary ping",
            &format!("Weekly canary on {host}: cookies probe ran and alerts still work."),
            2,
            &["bell"],
        )
    }
}
