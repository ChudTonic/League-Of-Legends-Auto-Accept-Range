//! Skin/hash download pipeline (S7) — see `docs/SKINS_PORT.md` §2. Ported
//! from `utils\download\repo_downloader.py` (`RepoDownloader`, the only
//! wired skin downloader) and `utils\download\hash_updater.py`
//! (`update_hash_files`, the wired hash-file updater). `SkinDownloader`/
//! `SmartSkinDownloader` (superseded by `RepoDownloader`) and
//! `hashes_downloader.py` (a dead duplicate of `hash_updater.py`) are NOT
//! ported — see `docs/SKINS_PORT.md` §0 scope decisions.
//!
//! This module holds the pieces shared by both downloaders: the error type,
//! the progress-callback signature, an HTTP client with Chud's User-Agent,
//! and the streaming-GET-with-retry helper (GitHub ZIP + hash shards both
//! hit the same anonymous, rate-limited GitHub infrastructure).

#![allow(dead_code)] // consumed by S9 (UI-driven download commands)
#![allow(unused_imports)] // the `pub use` re-exports land their first call sites in S9

pub mod hashes;
pub mod repo;

use std::path::Path;
use std::time::Duration;

use reqwest::StatusCode;

pub use hashes::ensure_hashes;
pub use repo::{download_skins, download_skins_incremental, DownloadMethod, DownloadOutcome};

use crate::skins::paths;
use crate::skins::slog::{log_info, log_warn};

/// Progress callback shared by every downloader here: `(done, total)`,
/// where `total` is `None` until the server reports a size (or never, for
/// the many-small-files incremental path). Ported from Python's
/// `ProgressCallback` type alias — there it also carried a status *string*
/// driving a Win32 dialog; that UI is dropped per `docs/SKINS_PORT.md` §0,
/// S9 renders its own progress bar off this numeric pair.
pub type Progress<'a> = &'a mut (dyn FnMut(u64, Option<u64>) + Send);

/// A generous safety-net timeout for the large streaming downloads (repo
/// ZIP, hash shards). NOTE this deliberately does NOT reuse Python's
/// `SKIN_DOWNLOAD_STREAM_TIMEOUT_S` (60s) as a `reqwest` per-request
/// timeout: `requests`' `timeout=` with `stream=True` bounds each
/// socket-level read (an inactivity timeout), not the whole transfer, so a
/// slow-but-progressing multi-minute download still succeeds in Python. A
/// literal `reqwest` `.timeout(60s)` bounds the *entire* request including
/// body, and would abort a legitimately slow ~200MB transfer. This is a
/// dead-connection guard, not a throughput floor.
const STREAM_SAFETY_TIMEOUT_S: u64 = 600;

#[derive(Debug)]
pub enum DownloadError {
    Io(std::io::Error),
    Http(reqwest::Error),
    Zip(zip::result::ZipError),
    Json(serde_json::Error),
    /// A hash shard's body wasn't valid UTF-8 (they're ported as plain text).
    Utf8(std::string::FromUtf8Error),
    /// GitHub's anonymous rate limit (60 requests/hr) — HTTP 403/429.
    RateLimited,
    Other(String),
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DownloadError::Io(e) => write!(f, "I/O error: {e}"),
            DownloadError::Http(e) => write!(f, "HTTP error: {e}"),
            DownloadError::Zip(e) => write!(f, "zip error: {e}"),
            DownloadError::Json(e) => write!(f, "JSON error: {e}"),
            DownloadError::Utf8(e) => write!(f, "invalid UTF-8: {e}"),
            DownloadError::RateLimited => write!(f, "GitHub rate limit exceeded (anonymous 60/hr)"),
            DownloadError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for DownloadError {}

impl From<std::io::Error> for DownloadError {
    fn from(e: std::io::Error) -> Self {
        DownloadError::Io(e)
    }
}
impl From<reqwest::Error> for DownloadError {
    fn from(e: reqwest::Error) -> Self {
        DownloadError::Http(e)
    }
}
impl From<zip::result::ZipError> for DownloadError {
    fn from(e: zip::result::ZipError) -> Self {
        DownloadError::Zip(e)
    }
}
impl From<serde_json::Error> for DownloadError {
    fn from(e: serde_json::Error) -> Self {
        DownloadError::Json(e)
    }
}
impl From<std::string::FromUtf8Error> for DownloadError {
    fn from(e: std::string::FromUtf8Error) -> Self {
        DownloadError::Utf8(e)
    }
}

/// `Chud/{version}` User-Agent header — see the rebrand map in
/// `docs/SKINS_PORT.md` §1.
fn user_agent() -> String {
    format!("Chud/{}", env!("CARGO_PKG_VERSION"))
}

pub(crate) fn build_client() -> Result<reqwest::Client, DownloadError> {
    Ok(reqwest::Client::builder().user_agent(user_agent()).build()?)
}

/// Stream `url` into memory, invoking `progress(baseline + done, total)` per
/// chunk. `total_hint` seeds the total (e.g. from a HEAD request) when the
/// response itself doesn't report `Content-Length`; `baseline` lets callers
/// merging multiple files (hash shards) report cumulative progress across
/// the whole sequence.
async fn stream_get(
    client: &reqwest::Client,
    url: &str,
    baseline: u64,
    total_hint: Option<u64>,
    progress: Progress<'_>,
) -> Result<Vec<u8>, DownloadError> {
    let mut response =
        client.get(url).timeout(Duration::from_secs(STREAM_SAFETY_TIMEOUT_S)).send().await?;
    let status = response.status();
    if status == StatusCode::FORBIDDEN || status == StatusCode::TOO_MANY_REQUESTS {
        return Err(DownloadError::RateLimited);
    }
    if !status.is_success() {
        return Err(DownloadError::Other(format!("HTTP {status} for {url}")));
    }

    let total = total_hint.or_else(|| response.content_length());
    let mut buf = Vec::new();
    let mut downloaded = 0u64;
    while let Some(chunk) = response.chunk().await? {
        buf.extend_from_slice(&chunk);
        downloaded += chunk.len() as u64;
        progress(baseline + downloaded, total);
    }
    Ok(buf)
}

/// Stream `url` with exponential-backoff retry (2s/4s/8s, 3 attempts total —
/// ported verbatim from `repo_downloader.py::download_repo_zip`'s retry
/// loop). GitHub's anonymous rate limit (403/429) is NOT retried — retrying
/// an hourly quota within seconds can't help, so it bails immediately with a
/// clear log line (mirrors the distinct rate-limit handling in
/// `hash_updater.py::check_file_commits`).
async fn stream_get_with_retry(
    client: &reqwest::Client,
    url: &str,
    baseline: u64,
    total_hint: Option<u64>,
    progress: Progress<'_>,
) -> Result<Vec<u8>, DownloadError> {
    const MAX_RETRIES: u32 = 3;
    const BASE_DELAY_S: u64 = 2;

    let mut last_err = DownloadError::Other(format!("no attempt made for {url}"));
    for attempt in 1..=MAX_RETRIES {
        match stream_get(client, url, baseline, total_hint, &mut *progress).await {
            Ok(bytes) => return Ok(bytes),
            Err(DownloadError::RateLimited) => {
                log_warn!("[DOWNLOADS] GitHub rate limit hit downloading {url}, not retrying");
                return Err(DownloadError::RateLimited);
            }
            Err(e) => {
                log_warn!("[DOWNLOADS] download attempt {attempt}/{MAX_RETRIES} failed for {url}: {e}");
                last_err = e;
                if attempt < MAX_RETRIES {
                    let delay = BASE_DELAY_S * 2u64.pow(attempt - 1); // 2s, 4s, 8s
                    log_info!("[DOWNLOADS] retrying in {delay}s...");
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                }
            }
        }
    }
    log_warn!("[DOWNLOADS] giving up on {url} after {MAX_RETRIES} attempts");
    Err(last_err)
}

/// Single-attempt GET-to-bytes with no chunked progress reporting — used for
/// the many small per-file downloads in the incremental skin-update path,
/// matching `repo_downloader.py::download_changed_files`'s plain
/// (non-retrying) `session.get(...)` per changed file.
async fn simple_get(client: &reqwest::Client, url: &str, timeout: Duration) -> Result<Vec<u8>, DownloadError> {
    let response = client.get(url).timeout(timeout).send().await?;
    let status = response.status();
    if status == StatusCode::FORBIDDEN || status == StatusCode::TOO_MANY_REQUESTS {
        return Err(DownloadError::RateLimited);
    }
    if !status.is_success() {
        return Err(DownloadError::Other(format!("HTTP {status} for {url}")));
    }
    Ok(response.bytes().await?.to_vec())
}

/// Decide incremental vs. full download and run it (ported from the
/// `needs_full_download` decision in
/// `launcher\sequences\skin_sync_sequence.py::perform_skin_sync`, which fed
/// `skin_downloader.py::download_skins_on_startup`). The Win32 dialog
/// plumbing and `AppStatus` tray-icon side effects around that decision are
/// dropped per `docs/SKINS_PORT.md` §0 — S9 drives its own UI off
/// `progress`.
pub async fn download_skins_on_startup(
    force: bool,
    progress: Progress<'_>,
) -> Result<DownloadOutcome, DownloadError> {
    let skins_dir = paths::skins_dir();
    let resources_dir = paths::resources_dir();

    let needs_full = force || !skins_present(&skins_dir);
    if needs_full {
        log_info!("[DOWNLOADS] performing full skin download (force={force})");
        download_skins(&skins_dir, &resources_dir, progress).await
    } else {
        log_info!("[DOWNLOADS] checking for incremental skin updates");
        download_skins_incremental(&skins_dir, &resources_dir, progress).await
    }
}

/// Cheap "is there anything downloaded" check standing in for the decision
/// half of `state\core\app_status.py::AppStatus.check_skins_downloaded` —
/// that class (tray-icon status management) is dropped per
/// `docs/SKINS_PORT.md` §0; only the "does a champion/skin folder with
/// actual content exist" question survives, to decide full vs. incremental.
fn skins_present(skins_dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(skins_dir) else { return false };
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let Ok(skin_dirs) = std::fs::read_dir(entry.path()) else { continue };
        for skin_dir in skin_dirs.flatten() {
            if skin_dir.path().is_dir() {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skins_present_false_when_dir_missing() {
        let dir = std::env::temp_dir().join("chud_downloads_test_missing_skins_dir");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!skins_present(&dir));
    }

    #[test]
    fn skins_present_true_when_a_skin_subfolder_exists() {
        let dir = std::env::temp_dir().join("chud_downloads_test_present_skins_dir");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("266").join("6660")).unwrap();
        assert!(skins_present(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skins_present_false_when_only_empty_champion_folder() {
        let dir = std::env::temp_dir().join("chud_downloads_test_empty_champ_dir");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("266")).unwrap();
        assert!(!skins_present(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
