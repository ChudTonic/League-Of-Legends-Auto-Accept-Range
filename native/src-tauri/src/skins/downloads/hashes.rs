//! Game hash table downloader — ported from `utils\download\hash_updater.py`
//! (`update_hash_files`), the wired hash-file updater
//! (`hashes_downloader.py` is a dead duplicate — not ported, per
//! `docs/SKINS_PORT.md` §0). Merges the 9 CommunityDragon
//! `hashes.game.txt.{0..8}` shards into one ~207MB `hashes.game.txt`,
//! re-downloading only when a per-file commits-API check shows upstream
//! changed.

#![allow(dead_code)] // consumed by S9 (UI-driven download commands)

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::skins::paths;
use crate::skins::slog::{log_info, log_warn};

use super::{DownloadError, Progress};

const GITHUB_API_BASE: &str = "https://api.github.com/repos/CommunityDragon/Data";
const GITHUB_RAW_BASE: &str = "https://raw.githubusercontent.com/CommunityDragon/Data/master";
const HASHES_DIR: &str = "hashes/lol";
/// Ported verbatim from `hash_updater.py::HASH_FILES = [f"hashes.game.txt.{i}" for i in range(9)]`.
const SHARD_COUNT: u32 = 9;
const TARGET_FILE: &str = "hashes.game.txt";
const STATE_FILE_NAME: &str = "hash_updater_state.json";

fn shard_name(index: u32) -> String {
    format!("{TARGET_FILE}.{index}")
}

fn shard_api_path(index: u32) -> String {
    format!("{HASHES_DIR}/{}", shard_name(index))
}

fn shard_url(index: u32) -> String {
    format!("{GITHUB_RAW_BASE}/{HASHES_DIR}/{}", shard_name(index))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct HashUpdaterState {
    #[serde(default)]
    files: HashMap<String, FileCommitInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileCommitInfo {
    sha: String,
    date: String,
}

/// Outcome of a single shard's commits-API check (ported from
/// `check_file_commits`'s three return shapes: a commit dict, `None`
/// (not-found/error), or `{'rate_limited': True}`).
enum CommitCheck {
    Found(FileCommitInfo),
    NotFound,
    RateLimited,
}

fn state_file_path() -> PathBuf {
    paths::state_dir().join(STATE_FILE_NAME)
}

fn load_state() -> HashUpdaterState {
    let path = state_file_path();
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            log_warn!("[DOWNLOADS] failed to parse hash updater state: {e}");
            HashUpdaterState::default()
        }),
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                log_warn!("[DOWNLOADS] failed to load hash updater state: {e}");
            }
            HashUpdaterState::default()
        }
    }
}

fn save_state(state: &HashUpdaterState) {
    let path = state_file_path();
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            log_warn!("[DOWNLOADS] failed to save hash updater state: {e}");
            return;
        }
    }
    match serde_json::to_string_pretty(state) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                log_warn!("[DOWNLOADS] failed to save hash updater state: {e}");
            }
        }
        Err(e) => log_warn!("[DOWNLOADS] failed to save hash updater state: {e}"),
    }
}

/// Check the latest commit for one shard's path in the CommunityDragon repo
/// (ported from `check_file_commits`).
async fn check_file_commit(client: &reqwest::Client, index: u32) -> CommitCheck {
    #[derive(Deserialize)]
    struct CommitEntry {
        sha: String,
        commit: CommitDetail,
    }
    #[derive(Deserialize)]
    struct CommitDetail {
        committer: Committer,
    }
    #[derive(Deserialize)]
    struct Committer {
        date: String,
    }

    let path = shard_api_path(index);
    let url = format!("{GITHUB_API_BASE}/commits");
    let resp = client
        .get(&url)
        .query(&[("path", path.as_str()), ("sha", "master"), ("per_page", "1")])
        .header(reqwest::header::ACCEPT, "application/vnd.github.v3+json")
        .timeout(Duration::from_secs(10))
        .send()
        .await;

    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            log_warn!("[DOWNLOADS] failed to check commits for {path}: {e}");
            return CommitCheck::NotFound;
        }
    };

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return CommitCheck::NotFound;
    }
    if resp.status() == reqwest::StatusCode::FORBIDDEN || resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        log_warn!("[DOWNLOADS] GitHub API rate limit exceeded while checking {path}");
        return CommitCheck::RateLimited;
    }
    if !resp.status().is_success() {
        log_warn!("[DOWNLOADS] failed to check commits for {path}: HTTP {}", resp.status());
        return CommitCheck::NotFound;
    }

    match resp.json::<Vec<CommitEntry>>().await {
        Ok(mut entries) if !entries.is_empty() => {
            let first = entries.remove(0);
            CommitCheck::Found(FileCommitInfo { sha: first.sha, date: first.commit.committer.date })
        }
        Ok(_) => CommitCheck::NotFound,
        Err(e) => {
            log_warn!("[DOWNLOADS] failed to parse commit info for {path}: {e}");
            CommitCheck::NotFound
        }
    }
}

/// Check whether any shard changed upstream (ported from `check_for_updates`).
async fn check_for_updates(client: &reqwest::Client, state: &HashUpdaterState) -> bool {
    let mut any_updated = false;
    let mut any_checked = false;
    let mut rate_limited = false;

    for i in 0..SHARD_COUNT {
        match check_file_commit(client, i).await {
            CommitCheck::RateLimited => rate_limited = true,
            CommitCheck::NotFound => {}
            CommitCheck::Found(info) => {
                any_checked = true;
                let shard = shard_name(i);
                match state.files.get(&shard) {
                    None => {
                        log_info!("[DOWNLOADS] no local state for {shard}, hash files will be downloaded");
                        return true;
                    }
                    Some(local) if local.sha != info.sha => {
                        log_info!(
                            "[DOWNLOADS] {shard} updated: {}.. -> {}..",
                            &local.sha[..8.min(local.sha.len())],
                            &info.sha[..8.min(info.sha.len())]
                        );
                        any_updated = true;
                    }
                    Some(_) => {}
                }
            }
        }
    }

    if rate_limited {
        log_warn!("[DOWNLOADS] rate limited on some hash files, cannot fully check for updates");
        return false;
    }
    if !any_checked {
        log_warn!("[DOWNLOADS] failed to check commits for any hash files");
        return false;
    }
    if any_updated {
        return true;
    }
    log_info!("[DOWNLOADS] hash files are up to date");
    false
}

/// Merge shard contents into one file: decode UTF-8, strip each shard's
/// trailing newlines, join with `\n`, and ensure exactly one trailing
/// newline overall (ported verbatim from `combine_hash_files`).
fn combine_hash_shards(shards: Vec<Vec<u8>>) -> Result<Vec<u8>, DownloadError> {
    let mut combined = String::new();
    for (i, bytes) in shards.into_iter().enumerate() {
        let text = String::from_utf8(bytes)?;
        if i > 0 {
            combined.push('\n');
        }
        combined.push_str(text.trim_end_matches('\n'));
    }
    if !combined.is_empty() && !combined.ends_with('\n') {
        combined.push('\n');
    }
    Ok(combined.into_bytes())
}

/// Check for updates and download/merge the hash shards into
/// `tools_dir/hashes.game.txt` if needed (ported from `update_hash_files`).
/// Returns `Ok(true)` if the file was (re)written, `Ok(false)` if the
/// existing file is already current.
pub async fn ensure_hashes(tools_dir: &Path, progress: Progress<'_>) -> Result<bool, DownloadError> {
    let target_path = tools_dir.join(TARGET_FILE);
    let target_exists = target_path.exists();

    let client = super::build_client()?;
    let state = load_state();

    if !check_for_updates(&client, &state).await {
        if target_exists {
            log_info!("[DOWNLOADS] game hashes are valid (no update needed)");
            return Ok(false);
        }
        log_warn!("[DOWNLOADS] hash file missing on disk, forcing download despite update check result");
    }

    log_info!("[DOWNLOADS] hash files have been updated, downloading...");
    std::fs::create_dir_all(tools_dir)?;

    let mut shard_bytes: Vec<Vec<u8>> = Vec::with_capacity(SHARD_COUNT as usize);
    let mut downloaded_total = 0u64;
    for i in 0..SHARD_COUNT {
        let name = shard_name(i);
        log_info!("[DOWNLOADS] downloading {name}...");
        let bytes = super::stream_get_with_retry(&client, &shard_url(i), downloaded_total, None, &mut *progress).await?;
        downloaded_total += bytes.len() as u64;
        log_info!("[DOWNLOADS] downloaded {name} ({:.1} MB)", bytes.len() as f64 / (1024.0 * 1024.0));
        shard_bytes.push(bytes);
    }

    log_info!("[DOWNLOADS] merging hashes files...");
    let combined = combine_hash_shards(shard_bytes)?;

    log_info!("[DOWNLOADS] writing {TARGET_FILE}...");
    std::fs::write(&target_path, &combined)?;
    let size_mb = combined.len() as f64 / (1024.0 * 1024.0);
    log_info!("[DOWNLOADS] successfully created {TARGET_FILE} ({size_mb:.1} MB)");

    // Refresh state with the new commit SHAs (best-effort — a failed
    // refresh just means the next run re-downloads unnecessarily).
    let mut new_state = HashUpdaterState::default();
    for i in 0..SHARD_COUNT {
        if let CommitCheck::Found(info) = check_file_commit(&client, i).await {
            new_state.files.insert(shard_name(i), info);
        }
    }
    if !new_state.files.is_empty() {
        save_state(&new_state);
    }

    log_info!("[DOWNLOADS] game hashes updated successfully");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_name_and_url_construction() {
        assert_eq!(shard_name(0), "hashes.game.txt.0");
        assert_eq!(shard_name(8), "hashes.game.txt.8");
        assert_eq!(
            shard_url(3),
            "https://raw.githubusercontent.com/CommunityDragon/Data/master/hashes/lol/hashes.game.txt.3"
        );
        assert_eq!(shard_api_path(5), "hashes/lol/hashes.game.txt.5");
    }

    #[test]
    fn shard_count_is_nine() {
        assert_eq!(SHARD_COUNT, 9);
    }

    #[test]
    fn combine_strips_trailing_newlines_and_joins() {
        let shards = vec![b"a\nb\n".to_vec(), b"c\n".to_vec(), b"d".to_vec()];
        let combined = combine_hash_shards(shards).unwrap();
        assert_eq!(combined, b"a\nb\nc\nd\n".to_vec());
    }

    #[test]
    fn combine_empty_input_yields_empty_output() {
        let combined = combine_hash_shards(vec![]).unwrap();
        assert!(combined.is_empty());
    }

    #[test]
    fn combine_rejects_invalid_utf8() {
        let shards = vec![vec![0xff, 0xfe, 0xfd]];
        assert!(combine_hash_shards(shards).is_err());
    }

    #[test]
    fn state_json_round_trip() {
        let mut state = HashUpdaterState::default();
        state.files.insert(
            "hashes.game.txt.0".to_string(),
            FileCommitInfo { sha: "abc123".to_string(), date: "2026-01-01T00:00:00Z".to_string() },
        );
        let json = serde_json::to_string(&state).unwrap();
        let parsed: HashUpdaterState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.files.get("hashes.game.txt.0").unwrap().sha, "abc123");
    }

    #[test]
    fn state_parses_missing_files_key_as_empty() {
        let parsed: HashUpdaterState = serde_json::from_str("{}").unwrap();
        assert!(parsed.files.is_empty());
    }
}
