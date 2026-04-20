use anyhow::{Context, Result};
use id3::{Tag, TagLike, Version};
use serde::Deserialize;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread::sleep;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use wait_timeout::ChildExt;

use crate::config::MusicBrainzConfig;

/// Minimum AcoustID score to accept a match. AcoustID scores are 0.0-1.0 where
/// 1.0 is a perfect fingerprint match. 0.85 avoids near-miss false positives
/// while still catching legitimate live/remix/remaster variants.
const ACOUSTID_SCORE_THRESHOLD: f64 = 0.85;

/// MusicBrainz requires identifying User-Agent: <app>/<version> ( <contact> )
const MB_USER_AGENT: &str = concat!(
    "ytsync-pi/",
    env!("CARGO_PKG_VERSION"),
    " ( https://github.com/jj-repository/ytsync-pi )"
);

/// MusicBrainz hard rate limit is 1 req/sec.
const MB_MIN_INTERVAL: Duration = Duration::from_millis(1100);

#[derive(Debug, Clone)]
pub struct EnrichedTags {
    pub artist: Option<String>,
    pub album: Option<String>,
    pub year: Option<i32>,
    pub track_number: Option<u32>,
    pub total_tracks: Option<u32>,
    pub genres: Vec<String>,
    pub recording_mbid: Option<String>,
    pub release_mbid: Option<String>,
}

#[derive(Debug)]
pub enum TagOutcome {
    Enriched(EnrichedTags),
    NoMatch,
    Skipped(String),
}

pub struct Tagger {
    cfg: MusicBrainzConfig,
    mb_last_call: Mutex<Option<Instant>>,
    http: ureq::Agent,
    fpcalc_available: bool,
}

impl Tagger {
    pub fn new(cfg: MusicBrainzConfig) -> Self {
        let fpcalc_available = Command::new("fpcalc")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !fpcalc_available {
            warn!(
                "fpcalc (Chromaprint) not found on PATH; MusicBrainz enrichment will be skipped. \
                 Install via `apt install libchromaprint-tools` or equivalent."
            );
        }
        Self {
            cfg,
            mb_last_call: Mutex::new(None),
            http: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .user_agent(MB_USER_AGENT)
                .build(),
            fpcalc_available,
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled && !self.cfg.acoustid_api_key.is_empty() && self.fpcalc_available
    }

    /// Enriches `path` (an MP3) with MusicBrainz-derived ID3 tags. Non-fatal on
    /// failure — logs the reason and returns the outcome so the caller can record it.
    pub fn tag_mp3(&self, path: &Path) -> TagOutcome {
        if !self.cfg.enabled {
            return TagOutcome::Skipped("MusicBrainz disabled in config".into());
        }
        if self.cfg.acoustid_api_key.is_empty() {
            return TagOutcome::Skipped("acoustid_api_key is empty".into());
        }
        if !self.fpcalc_available {
            return TagOutcome::Skipped("fpcalc not installed".into());
        }

        let (duration, fingerprint) = match fingerprint(path) {
            Ok(v) => v,
            Err(e) => return TagOutcome::Skipped(format!("fpcalc failed: {e}")),
        };
        debug!(
            "fpcalc ok for {}: duration={}s fp={} chars",
            path.display(),
            duration,
            fingerprint.len()
        );

        let mat = match self.acoustid_lookup(duration, &fingerprint) {
            Ok(Some(m)) => m,
            Ok(None) => return TagOutcome::NoMatch,
            Err(e) => return TagOutcome::Skipped(format!("AcoustID lookup failed: {e}")),
        };
        info!(
            "AcoustID match for {}: score={:.2} artist={:?} title={:?}",
            path.display(),
            mat.score,
            mat.artist,
            mat.title
        );

        let genres = match self.mb_genres(&mat.recording_mbid) {
            Ok(g) => g,
            Err(e) => {
                warn!(
                    "MusicBrainz genre lookup failed for {}: {e}",
                    path.display()
                );
                Vec::new()
            }
        };

        let tags = EnrichedTags {
            artist: mat.artist.clone(),
            album: mat.album.clone(),
            year: mat.year,
            track_number: mat.track_number,
            total_tracks: mat.total_tracks,
            genres,
            recording_mbid: Some(mat.recording_mbid.clone()),
            release_mbid: mat.release_mbid.clone(),
        };

        if let Err(e) = write_id3(path, &tags) {
            return TagOutcome::Skipped(format!("ID3 write failed: {e}"));
        }
        TagOutcome::Enriched(tags)
    }

    fn acoustid_lookup(&self, duration: u32, fingerprint: &str) -> Result<Option<AcoustIdMatch>> {
        let url = "https://api.acoustid.org/v2/lookup";
        let resp = self
            .http
            .post(url)
            .send_form(&[
                ("client", self.cfg.acoustid_api_key.as_str()),
                ("duration", &duration.to_string()),
                ("fingerprint", fingerprint),
                ("meta", "recordings+releasegroups+tracks+releases+compress"),
            ])
            .context("AcoustID request")?;
        let body: AcoustIdResponse = read_json_capped(resp).context("decode AcoustID response")?;

        if body.status != "ok" {
            anyhow::bail!(
                "AcoustID status={}, error={:?}",
                body.status,
                body.error.map(|e| e.message)
            );
        }

        let best = body
            .results
            .into_iter()
            .filter(|r| r.score >= ACOUSTID_SCORE_THRESHOLD)
            .flat_map(|r| {
                let score = r.score;
                r.recordings.into_iter().map(move |rec| (score, rec))
            })
            .next();

        let Some((score, rec)) = best else {
            return Ok(None);
        };

        let artist = rec
            .artists
            .as_ref()
            .and_then(|a| a.first())
            .map(|a| a.name.clone());

        let (album, year, track_number, total_tracks, release_mbid) = rec
            .releases
            .as_ref()
            .and_then(|rels| rels.first())
            .map(|rel| {
                let tn = rel
                    .mediums
                    .as_ref()
                    .and_then(|m| m.first())
                    .and_then(|m| m.tracks.as_ref())
                    .and_then(|t| t.first())
                    .and_then(|t| t.position);
                let tt = rel
                    .mediums
                    .as_ref()
                    .and_then(|m| m.first())
                    .and_then(|m| m.track_count);
                (
                    rel.title.clone(),
                    rel.date.as_deref().and_then(parse_year),
                    tn,
                    tt,
                    Some(rel.id.clone()),
                )
            })
            .unwrap_or((None, None, None, None, None));

        Ok(Some(AcoustIdMatch {
            score,
            recording_mbid: rec.id,
            title: rec.title,
            artist,
            album,
            year,
            track_number,
            total_tracks,
            release_mbid,
        }))
    }

    fn mb_genres(&self, recording_mbid: &str) -> Result<Vec<String>> {
        self.throttle_mb();
        let url =
            format!("https://musicbrainz.org/ws/2/recording/{recording_mbid}?inc=genres&fmt=json");
        let resp = self.http.get(&url).call().context("MusicBrainz request")?;
        let body: MbRecording = read_json_capped(resp).context("decode MusicBrainz response")?;

        let mut genres: Vec<_> = body
            .genres
            .unwrap_or_default()
            .into_iter()
            .filter(|g| g.count.unwrap_or(0) > 0)
            .collect();
        genres.sort_by(|a, b| b.count.unwrap_or(0).cmp(&a.count.unwrap_or(0)));
        Ok(genres.into_iter().take(3).map(|g| g.name).collect())
    }

    fn throttle_mb(&self) {
        // Recover from poison: the stored Instant is safe to reuse even if a
        // prior holder panicked, and we'd rather keep tagging running than abort
        // the whole run over a harmless mutex.
        let mut guard = self.mb_last_call.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(last) = *guard {
            let elapsed = Instant::now().saturating_duration_since(last);
            if elapsed < MB_MIN_INTERVAL {
                sleep(MB_MIN_INTERVAL - elapsed);
            }
        }
        *guard = Some(Instant::now());
    }
}

#[derive(Debug, Clone)]
struct AcoustIdMatch {
    score: f64,
    recording_mbid: String,
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    year: Option<i32>,
    track_number: Option<u32>,
    total_tracks: Option<u32>,
    release_mbid: Option<String>,
}

// --- fpcalc ---

/// fpcalc timeout. A healthy run on a 5-minute MP3 is under 2s on a Pi; 60s
/// is a generous ceiling that still prevents a corrupted file from wedging the
/// entire sync.
const FPCALC_TIMEOUT: Duration = Duration::from_secs(60);

/// Cap on any single HTTP response body. AcoustID and MusicBrainz responses
/// for a single recording are well under 100 KiB in practice; 2 MiB leaves
/// plenty of headroom while stopping a malicious or broken endpoint from
/// buffering unbounded data into RAM.
const HTTP_RESPONSE_CAP: u64 = 2 * 1024 * 1024;

fn read_json_capped<T>(resp: ureq::Response) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    use std::io::Read;
    let mut buf = Vec::with_capacity(16 * 1024);
    resp.into_reader()
        .take(HTTP_RESPONSE_CAP + 1)
        .read_to_end(&mut buf)
        .context("read HTTP body")?;
    if buf.len() as u64 > HTTP_RESPONSE_CAP {
        anyhow::bail!("HTTP response exceeded {} bytes", HTTP_RESPONSE_CAP);
    }
    serde_json::from_slice(&buf).context("decode JSON")
}

fn fingerprint(path: &Path) -> Result<(u32, String)> {
    let mut child = Command::new("fpcalc")
        .arg("-json")
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn fpcalc")?;

    let status = match child.wait_timeout(FPCALC_TIMEOUT) {
        Ok(Some(s)) => s,
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "fpcalc timed out after {}s on {}",
                FPCALC_TIMEOUT.as_secs(),
                path.display()
            );
        }
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("fpcalc wait_timeout error: {e}");
        }
    };

    let out = child.wait_with_output().context("collect fpcalc output")?;
    if !status.success() {
        anyhow::bail!(
            "fpcalc exited {}: {}",
            status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let parsed: FpcalcOutput =
        serde_json::from_slice(&out.stdout).context("parse fpcalc -json output")?;
    Ok((parsed.duration.round() as u32, parsed.fingerprint))
}

// --- ID3 writer ---

fn write_id3(path: &Path, tags: &EnrichedTags) -> Result<()> {
    let mut id3 = match Tag::read_from_path(path) {
        Ok(t) => t,
        Err(id3::Error {
            kind: id3::ErrorKind::NoTag,
            ..
        }) => Tag::new(),
        Err(e) => return Err(e).context("read existing ID3 tag"),
    };

    // Preserve title if yt-dlp's --embed-metadata already set it.
    if id3.title().is_none() {
        // deliberately leave unset — filename is still the source of truth.
    }

    if let Some(a) = &tags.artist {
        id3.set_artist(a);
    }
    if let Some(a) = &tags.album {
        id3.set_album(a);
    }
    if let Some(y) = tags.year {
        id3.set_year(y);
    }
    if let Some(tn) = tags.track_number {
        id3.set_track(tn);
    }
    if let Some(tt) = tags.total_tracks {
        id3.set_total_tracks(tt);
    }
    if !tags.genres.is_empty() {
        id3.set_genre(tags.genres.join("; "));
    }
    if let Some(mbid) = &tags.recording_mbid {
        id3.add_frame(id3::frame::ExtendedText {
            description: "MusicBrainz Recording Id".into(),
            value: mbid.clone(),
        });
    }
    if let Some(mbid) = &tags.release_mbid {
        id3.add_frame(id3::frame::ExtendedText {
            description: "MusicBrainz Album Id".into(),
            value: mbid.clone(),
        });
    }

    // Keep the existing thumbnail yt-dlp embedded; we don't overwrite pictures here.
    id3.write_to_path(path, Version::Id3v24)
        .context("write ID3 tag")?;
    Ok(())
}

fn parse_year(date: &str) -> Option<i32> {
    date.split('-').next()?.parse::<i32>().ok()
}

// --- AcoustID JSON shapes (only the fields we consume) ---

#[derive(Debug, Deserialize)]
struct AcoustIdResponse {
    status: String,
    #[serde(default)]
    error: Option<AcoustIdError>,
    #[serde(default)]
    results: Vec<AcoustIdResult>,
}

#[derive(Debug, Deserialize)]
struct AcoustIdError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct AcoustIdResult {
    score: f64,
    #[serde(default)]
    recordings: Vec<AcoustIdRecording>,
}

#[derive(Debug, Deserialize)]
struct AcoustIdRecording {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    artists: Option<Vec<AcoustIdArtist>>,
    #[serde(default)]
    releases: Option<Vec<AcoustIdRelease>>,
}

#[derive(Debug, Deserialize)]
struct AcoustIdArtist {
    name: String,
}

#[derive(Debug, Deserialize)]
struct AcoustIdRelease {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    mediums: Option<Vec<AcoustIdMedium>>,
}

#[derive(Debug, Deserialize)]
struct AcoustIdMedium {
    #[serde(default, rename = "track_count")]
    track_count: Option<u32>,
    #[serde(default)]
    tracks: Option<Vec<AcoustIdTrack>>,
}

#[derive(Debug, Deserialize)]
struct AcoustIdTrack {
    #[serde(default)]
    position: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct FpcalcOutput {
    duration: f64,
    fingerprint: String,
}

// --- MusicBrainz JSON shapes ---

#[derive(Debug, Deserialize)]
struct MbRecording {
    #[serde(default)]
    genres: Option<Vec<MbGenre>>,
}

#[derive(Debug, Deserialize)]
struct MbGenre {
    name: String,
    #[serde(default)]
    count: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_year_from_mb_date() {
        assert_eq!(parse_year("2019-04-15"), Some(2019));
        assert_eq!(parse_year("2019"), Some(2019));
        assert_eq!(parse_year(""), None);
        assert_eq!(parse_year("unknown"), None);
    }
}
