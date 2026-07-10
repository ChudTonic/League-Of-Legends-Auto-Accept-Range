//! Skin repository downloader — ported from `utils\download\repo_downloader.py`
//! (`RepoDownloader`), the only wired skin downloader (`SkinDownloader`/
//! `SmartSkinDownloader` are superseded and dropped per
//! `docs/SKINS_PORT.md` §0). Full download fetches the repo ZIP and
//! extracts only the `skins/` and `resources/` archive prefixes; incremental
//! update walks the GitHub compare API for a smaller diff when possible,
//! falling back to the full ZIP.

#![allow(dead_code)] // consumed by S9 (UI-driven download commands)

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::skins::slog::{log_info, log_warn};

use super::{DownloadError, Progress};

/// Upstream skin-data repository — the app's data dependency (skin
/// archives, previews, and the skin-name -> id map), not Chud branding.
/// // data source, not branding
const REPO_URL: &str = "https://github.com/Alban1911/LeagueSkins";
const API_BASE: &str = "https://api.github.com/repos/Alban1911/LeagueSkins";
const RAW_BASE: &str = "https://raw.githubusercontent.com/Alban1911/LeagueSkins/main";

/// Archive-root prefix GitHub's codeload ZIP names its top-level folder
/// (ported verbatim from `repo_downloader.py`'s `'LeagueSkins-main/'`
/// literal — tied to the `main` branch used in `download_repo_zip`'s URL).
const ARCHIVE_ROOT_PREFIX: &str = "LeagueSkins-main/";

/// Local commit-SHA tracking file, stored next to the downloaded data
/// (ported from `RepoDownloader.version_file` = `target_dir / '.skin_version'`).
const VERSION_FILE_NAME: &str = ".skin_version";

/// Streaming timeout for individual incremental-file GETs and the
/// short-lived JSON API calls (ported from `config.SKIN_DOWNLOAD_STREAM_TIMEOUT_S`).
const STREAM_TIMEOUT_S: u64 = 60;

/// Above this many changed files, fall back to a full ZIP download instead
/// of individual requests (ported verbatim from
/// `RepoDownloader.incremental_file_threshold`).
const INCREMENTAL_FILE_THRESHOLD: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadMethod {
    Full,
    Incremental,
    UpToDate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadOutcome {
    pub updated: bool,
    pub method: DownloadMethod,
}

struct ChangedFile {
    filename: String,
    status: String,
    previous_filename: Option<String>,
}

enum EntryKind {
    Skin(String),
    Resource(String),
    Other,
}

struct ExtractStats {
    skin_entries: usize,
    resource_entries: usize,
}

// ---------------------------------------------------------------------------
// Full download (ported from `RepoDownloader.download_and_extract_skins`)
// ---------------------------------------------------------------------------

/// Download the full repository ZIP and extract `skins/` + `resources/`
/// into `skins_dir` / `resources_dir`, overwriting existing files (mirrors
/// `download_and_extract_skins(force_update=True)`'s call site — the only
/// place Python's `overwrite_existing` knob is ever set `True`).
pub async fn download_skins(
    skins_dir: &Path,
    resources_dir: &Path,
    progress: Progress<'_>,
) -> Result<DownloadOutcome, DownloadError> {
    std::fs::create_dir_all(skins_dir)?;
    std::fs::create_dir_all(resources_dir)?;

    // Remove a stray non-directory `skins` file left behind by very old
    // installs (ported from `download_and_extract_skins`'s guard).
    let stray = skins_dir.join("skins");
    if stray.is_file() {
        log_info!("[DOWNLOADS] removing conflicting 'skins' file...");
        let _ = std::fs::remove_file(&stray);
    }

    let client = super::build_client()?;

    log_info!("[DOWNLOADS] downloading repository ZIP from {REPO_URL}");
    let zip_bytes = download_repo_zip(&client, &mut *progress).await?;

    log_info!("[DOWNLOADS] extracting skins, previews, and resources from repository ZIP...");
    let stats = extract_and_cleanup(zip_bytes, skins_dir, resources_dir)?;
    log_info!(
        "[DOWNLOADS] extracted {} skin files and {} resource files",
        stats.skin_entries, stats.resource_entries
    );

    if let Some(sha) = fetch_remote_sha(&client).await? {
        save_local_sha(skins_dir, &sha);
    }

    Ok(DownloadOutcome { updated: true, method: DownloadMethod::Full })
}

async fn download_repo_zip(client: &reqwest::Client, progress: Progress<'_>) -> Result<Vec<u8>, DownloadError> {
    let zip_url = format!("{REPO_URL}/archive/refs/heads/main.zip");
    let total_hint = head_content_length(client, &zip_url).await;
    super::stream_get_with_retry(client, &zip_url, 0, total_hint, progress).await
}

async fn head_content_length(client: &reqwest::Client, url: &str) -> Option<u64> {
    let resp = client.head(url).timeout(Duration::from_secs(STREAM_TIMEOUT_S)).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.content_length()
}

// ---------------------------------------------------------------------------
// Extraction (ported from `RepoDownloader.extract_skins_from_zip` +
// `_cleanup_removed_skin_files`)
// ---------------------------------------------------------------------------

/// Classify a ZIP member by its (possibly archive-root-prefixed) path,
/// stripping the `LeagueSkins-main/` prefix and then the `skins/`/
/// `resources/` prefix — pure and unit-testable.
fn classify_entry(name: &str) -> EntryKind {
    let stripped = name.strip_prefix(ARCHIVE_ROOT_PREFIX).unwrap_or(name);
    if let Some(rest) = stripped.strip_prefix("skins/") {
        if !rest.is_empty() && is_safe_relative_path(rest) {
            return EntryKind::Skin(rest.to_string());
        }
    } else if let Some(rest) = stripped.strip_prefix("resources/") {
        if !rest.is_empty() && is_safe_relative_path(rest) {
            return EntryKind::Resource(rest.to_string());
        }
    }
    EntryKind::Other
}

/// Defense-in-depth zip-slip guard: rejects absolute paths and any
/// `..`/`.` component. Not part of the literal Python (which trusts the
/// upstream ZIP), but cheap and consistent with this codebase's other
/// archive-handling code (`injection::zips::safe_extractall`).
fn is_safe_relative_path(rel: &str) -> bool {
    let path = Path::new(rel);
    !path.is_absolute() && path.components().all(|c| matches!(c, std::path::Component::Normal(_)))
}

fn extract_and_cleanup(
    zip_bytes: Vec<u8>,
    skins_dir: &Path,
    resources_dir: &Path,
) -> Result<ExtractStats, DownloadError> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes))?;

    let mut skin_entries: Vec<(usize, String)> = Vec::new();
    let mut resource_entries: Vec<(usize, String)> = Vec::new();

    for i in 0..archive.len() {
        let entry = archive.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_string();
        match classify_entry(&name) {
            EntryKind::Skin(rel) => skin_entries.push((i, rel)),
            EntryKind::Resource(rel) => resource_entries.push((i, rel)),
            EntryKind::Other => {}
        }
    }

    if skin_entries.is_empty() && resource_entries.is_empty() {
        return Err(DownloadError::Other("no skins or resources folder found in repository ZIP".into()));
    }

    for (index, rel) in &skin_entries {
        extract_one(&mut archive, *index, skins_dir, rel)?;
    }
    for (index, rel) in &resource_entries {
        extract_one(&mut archive, *index, resources_dir, rel)?;
    }

    let skin_rel_set: HashSet<String> = skin_entries.iter().map(|(_, rel)| normalize_rel(rel)).collect();
    cleanup_removed(skins_dir, &skin_rel_set, !skin_entries.is_empty());

    let resource_rel_set: HashSet<String> = resource_entries.iter().map(|(_, rel)| normalize_rel(rel)).collect();
    cleanup_removed(resources_dir, &resource_rel_set, !resource_entries.is_empty());

    Ok(ExtractStats { skin_entries: skin_entries.len(), resource_entries: resource_entries.len() })
}

fn extract_one<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    index: usize,
    target_root: &Path,
    rel_path: &str,
) -> Result<(), DownloadError> {
    let mut entry = archive.by_index(index)?;
    let dest = target_root.join(rel_path);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = std::fs::File::create(&dest)?;
    std::io::copy(&mut entry, &mut out)?;
    Ok(())
}

fn normalize_rel(rel: &str) -> String {
    rel.replace('\\', "/").to_lowercase()
}

/// Delete local files under `dir` that are absent from `expected_rel` — the
/// ZIP's current file list. Guarded against wiping everything when the ZIP
/// contained no matching files at all (ported verbatim from
/// `_cleanup_removed_skin_files`'s empty-list safety check — the whole
/// reason this function takes a bool rather than inferring emptiness from
/// `expected_rel` itself, since a non-empty ZIP list can still normalize to
/// an empty set if every entry was a directory marker).
fn cleanup_removed(dir: &Path, expected_rel: &HashSet<String>, have_entries: bool) {
    if !have_entries {
        log_info!(
            "[DOWNLOADS] skipping cleanup for {}: no matching files in ZIP (would wipe everything)",
            dir.display()
        );
        return;
    }
    if !dir.exists() {
        return;
    }

    let mut deleted = 0u32;
    for file in walk_files(dir) {
        // Skip state-tracking files (ported verbatim from the Python guard;
        // note it doesn't actually match `.skin_version` by name — harmless
        // here since `download_skins` always rewrites the SHA file again
        // right after this cleanup pass completes).
        if let Some(name) = file.file_name().and_then(|n| n.to_str()) {
            if name.starts_with('.') && name.ends_with("_state.json") {
                continue;
            }
        }
        let Ok(rel) = file.strip_prefix(dir) else { continue };
        let rel_str = normalize_rel(&rel.to_string_lossy());
        if !expected_rel.contains(&rel_str) {
            if std::fs::remove_file(&file).is_ok() {
                deleted += 1;
            }
        }
    }
    if deleted > 0 {
        log_info!("[DOWNLOADS] removed {deleted} obsolete files from {}", dir.display());
    }
    remove_empty_dirs(dir);
}

fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_files_into(dir, &mut out);
    out
}

fn walk_files_into(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_files_into(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn remove_empty_dirs(root: &Path) {
    let mut dirs = Vec::new();
    walk_dirs_into(root, &mut dirs);
    // Deepest paths sort lexicographically greatest (they're the longest,
    // most-specific strings), so reverse-sorting visits children before
    // their parents — same ordering Python's `sorted(..., reverse=True)`
    // relies on in `_cleanup_removed_skin_files`.
    dirs.sort_by(|a, b| b.cmp(a));
    for dir in dirs {
        if let Ok(mut it) = std::fs::read_dir(&dir) {
            if it.next().is_none() {
                let _ = std::fs::remove_dir(&dir);
            }
        }
    }
}

fn walk_dirs_into(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_dirs_into(&path, out);
            out.push(path);
        }
    }
}

// ---------------------------------------------------------------------------
// Version tracking (ported from `RepoDownloader.{get_local_sha,save_local_sha,fetch_remote_sha}`)
// ---------------------------------------------------------------------------

fn version_file_path(skins_dir: &Path) -> PathBuf {
    skins_dir.join(VERSION_FILE_NAME)
}

fn load_local_sha(skins_dir: &Path) -> Option<String> {
    match std::fs::read_to_string(version_file_path(skins_dir)) {
        Ok(s) => Some(s.trim().to_string()),
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                log_warn!("[DOWNLOADS] failed to read local skin SHA: {e}");
            }
            None
        }
    }
}

fn save_local_sha(skins_dir: &Path, sha: &str) {
    let path = version_file_path(skins_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&path, sha) {
        log_warn!("[DOWNLOADS] failed to save local skin SHA: {e}");
    }
}

/// Fetch the latest commit SHA from the skin repo (1 API call). Never hard
/// fails — mirrors Python's `fetch_remote_sha`, which logs a warning and
/// returns `None` on any request error rather than propagating.
async fn fetch_remote_sha(client: &reqwest::Client) -> Result<Option<String>, DownloadError> {
    #[derive(serde::Deserialize)]
    struct CommitSha {
        sha: String,
    }

    let url = format!("{API_BASE}/commits/main");
    let resp = client
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/vnd.github.v3+json")
        .timeout(Duration::from_secs(10))
        .send()
        .await;

    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            log_warn!("[DOWNLOADS] failed to fetch remote skin SHA: {e}");
            return Ok(None);
        }
    };
    if !resp.status().is_success() {
        log_warn!("[DOWNLOADS] failed to fetch remote skin SHA: HTTP {}", resp.status());
        return Ok(None);
    }
    match resp.json::<CommitSha>().await {
        Ok(c) => {
            log_info!("[DOWNLOADS] remote skin SHA: {}", &c.sha[..8.min(c.sha.len())]);
            Ok(Some(c.sha))
        }
        Err(e) => {
            log_warn!("[DOWNLOADS] failed to parse remote skin SHA response: {e}");
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------
// Incremental update (ported from `RepoDownloader.download_incremental_updates`
// + `get_changed_files` + `download_changed_files`)
// ---------------------------------------------------------------------------

/// Check for updates via commit SHA and download incrementally when
/// possible, falling back to a full ZIP (ported from
/// `download_incremental_updates`).
pub async fn download_skins_incremental(
    skins_dir: &Path,
    resources_dir: &Path,
    progress: Progress<'_>,
) -> Result<DownloadOutcome, DownloadError> {
    std::fs::create_dir_all(skins_dir)?;
    std::fs::create_dir_all(resources_dir)?;

    let client = super::build_client()?;

    let Some(remote_sha) = fetch_remote_sha(&client).await? else {
        log_warn!("[DOWNLOADS] could not fetch remote skin SHA, assuming no changes");
        progress(1, Some(1));
        return Ok(DownloadOutcome { updated: false, method: DownloadMethod::UpToDate });
    };

    let local_sha = load_local_sha(skins_dir);

    if local_sha.as_deref() == Some(remote_sha.as_str()) {
        log_info!("[DOWNLOADS] skin repository unchanged, skipping download");
        progress(1, Some(1));
        return Ok(DownloadOutcome { updated: false, method: DownloadMethod::UpToDate });
    }

    if let Some(local_sha) = &local_sha {
        if let Some(changed) = fetch_changed_files(&client, local_sha, &remote_sha).await? {
            if !changed.is_empty() && changed.len() <= INCREMENTAL_FILE_THRESHOLD {
                log_info!("[DOWNLOADS] incremental update: {} changed files", changed.len());
                match download_changed_files(&client, &changed, skins_dir, resources_dir, &mut *progress).await {
                    Ok(()) => {
                        save_local_sha(skins_dir, &remote_sha);
                        return Ok(DownloadOutcome { updated: true, method: DownloadMethod::Incremental });
                    }
                    Err(e) => {
                        log_warn!("[DOWNLOADS] incremental update had failures ({e}), falling back to full ZIP");
                    }
                }
            } else if changed.len() > INCREMENTAL_FILE_THRESHOLD {
                log_info!("[DOWNLOADS] too many changed files ({}), using full ZIP", changed.len());
            }
        }
    }

    download_skins(skins_dir, resources_dir, progress).await
}

/// Fetch changed files between two commits via the GitHub compare API.
/// Returns `None` (never `Err`) on any failure — mirrors Python's
/// `get_changed_files`, whose caller falls back to a full ZIP either way.
async fn fetch_changed_files(
    client: &reqwest::Client,
    old_sha: &str,
    new_sha: &str,
) -> Result<Option<Vec<ChangedFile>>, DownloadError> {
    #[derive(serde::Deserialize)]
    struct CompareResponse {
        #[serde(default)]
        files: Vec<CompareFile>,
    }
    #[derive(serde::Deserialize)]
    struct CompareFile {
        filename: String,
        status: String,
        previous_filename: Option<String>,
    }

    let url = format!("{API_BASE}/compare/{old_sha}...{new_sha}");
    let resp = client.get(&url).timeout(Duration::from_secs(20)).send().await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            log_warn!("[DOWNLOADS] compare API failed, will use full ZIP: {e}");
            return Ok(None);
        }
    };
    if resp.status() == reqwest::StatusCode::FORBIDDEN || resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        log_warn!("[DOWNLOADS] GitHub API rate limit hit on compare, will use full ZIP");
        return Ok(None);
    }
    if !resp.status().is_success() {
        log_warn!("[DOWNLOADS] compare API returned HTTP {}, will use full ZIP", resp.status());
        return Ok(None);
    }

    match resp.json::<CompareResponse>().await {
        Ok(c) => {
            log_info!(
                "[DOWNLOADS] compare API: {} changed files between {}..{}",
                c.files.len(),
                &old_sha[..8.min(old_sha.len())],
                &new_sha[..8.min(new_sha.len())]
            );
            Ok(Some(
                c.files
                    .into_iter()
                    .map(|f| ChangedFile {
                        filename: f.filename,
                        status: f.status,
                        previous_filename: f.previous_filename,
                    })
                    .collect(),
            ))
        }
        Err(e) => {
            log_warn!("[DOWNLOADS] failed to parse compare API response, will use full ZIP: {e}");
            Ok(None)
        }
    }
}

/// Map a repo-relative path (`skins/...` or `resources/...`) to a local
/// path (ported from `RepoDownloader._resolve_local_path`).
fn resolve_local_path(repo_path: &str, skins_dir: &Path, resources_dir: &Path) -> Option<PathBuf> {
    if let Some(rest) = repo_path.strip_prefix("skins/") {
        return (!rest.is_empty() && is_safe_relative_path(rest)).then(|| skins_dir.join(rest));
    }
    if let Some(rest) = repo_path.strip_prefix("resources/") {
        return (!rest.is_empty() && is_safe_relative_path(rest)).then(|| resources_dir.join(rest));
    }
    None
}

/// Download changed files individually via `raw.githubusercontent.com`.
/// Handles `skins/`/`resources/` paths and add/modify/remove/rename status
/// (ported from `RepoDownloader.download_changed_files`).
async fn download_changed_files(
    client: &reqwest::Client,
    changed: &[ChangedFile],
    skins_dir: &Path,
    resources_dir: &Path,
    progress: Progress<'_>,
) -> Result<(), DownloadError> {
    let total = changed.len() as u64;
    let mut success_count = 0u32;
    let mut fail_count = 0u32;
    let mut dirs_to_check: HashSet<PathBuf> = HashSet::new();

    for (idx, file) in changed.iter().enumerate() {
        let Some(local_path) = resolve_local_path(&file.filename, skins_dir, resources_dir) else {
            continue;
        };

        if file.status == "renamed" {
            if let Some(prev) = &file.previous_filename {
                if let Some(old_path) = resolve_local_path(prev, skins_dir, resources_dir) {
                    if old_path.exists() && std::fs::remove_file(&old_path).is_ok() {
                        if let Some(parent) = old_path.parent() {
                            dirs_to_check.insert(parent.to_path_buf());
                        }
                    }
                }
            }
        }

        if file.status == "removed" {
            if local_path.exists() && std::fs::remove_file(&local_path).is_ok() {
                if let Some(parent) = local_path.parent() {
                    dirs_to_check.insert(parent.to_path_buf());
                }
            }
            success_count += 1;
            progress(idx as u64 + 1, Some(total));
            continue;
        }

        let raw_url = format!("{RAW_BASE}/{}", file.filename);
        match super::simple_get(client, &raw_url, Duration::from_secs(STREAM_TIMEOUT_S)).await {
            Ok(bytes) => {
                if let Some(parent) = local_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::write(&local_path, &bytes) {
                    Ok(()) => success_count += 1,
                    Err(e) => {
                        log_warn!("[DOWNLOADS] failed to write {}: {e}", file.filename);
                        fail_count += 1;
                    }
                }
            }
            Err(e) => {
                log_warn!("[DOWNLOADS] failed to download {}: {e}", file.filename);
                fail_count += 1;
            }
        }
        progress(idx as u64 + 1, Some(total));
    }

    // Clean up empty directories left by removals/renames.
    let mut dirs: Vec<PathBuf> = dirs_to_check.into_iter().collect();
    dirs.sort_by(|a, b| b.cmp(a));
    for mut dir in dirs {
        loop {
            if dir == skins_dir || dir == resources_dir || !dir.exists() {
                break;
            }
            match std::fs::read_dir(&dir) {
                Ok(mut it) => {
                    if it.next().is_some() {
                        break;
                    }
                    let _ = std::fs::remove_dir(&dir);
                    match dir.parent() {
                        Some(parent) => dir = parent.to_path_buf(),
                        None => break,
                    }
                }
                Err(_) => break,
            }
        }
    }

    log_info!("[DOWNLOADS] incremental update: {success_count} succeeded, {fail_count} failed out of {total}");
    if fail_count == 0 {
        Ok(())
    } else {
        Err(DownloadError::Other(format!("{fail_count}/{total} incremental file downloads failed")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_strips_archive_root_and_skins_prefix() {
        match classify_entry("LeagueSkins-main/skins/266/6660/6660.zip") {
            EntryKind::Skin(rel) => assert_eq!(rel, "266/6660/6660.zip"),
            _ => panic!("expected Skin"),
        }
    }

    #[test]
    fn classify_strips_archive_root_and_resources_prefix() {
        match classify_entry("LeagueSkins-main/resources/skin_ids.json") {
            EntryKind::Resource(rel) => assert_eq!(rel, "skin_ids.json"),
            _ => panic!("expected Resource"),
        }
    }

    #[test]
    fn classify_directory_marker_is_other() {
        assert!(matches!(classify_entry("LeagueSkins-main/skins/"), EntryKind::Other));
        assert!(matches!(classify_entry("LeagueSkins-main/README.md"), EntryKind::Other));
    }

    #[test]
    fn classify_rejects_traversal_in_archive() {
        assert!(matches!(classify_entry("LeagueSkins-main/skins/../../evil.exe"), EntryKind::Other));
    }

    #[test]
    fn classify_without_archive_root_prefix_still_works() {
        // Defensive: if a future branch rename changes the archive root,
        // an already-stripped name should still classify correctly.
        match classify_entry("skins/1/1.zip") {
            EntryKind::Skin(rel) => assert_eq!(rel, "1/1.zip"),
            _ => panic!("expected Skin"),
        }
    }

    #[test]
    fn normalize_rel_lowercases_and_flips_separators() {
        assert_eq!(normalize_rel("266\\6660\\6660.ZIP"), "266/6660/6660.zip");
    }

    #[test]
    fn version_file_round_trip() {
        let dir = std::env::temp_dir().join("chud_repo_test_version_file");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        assert_eq!(load_local_sha(&dir), None);
        save_local_sha(&dir, "abc123");
        assert_eq!(load_local_sha(&dir), Some("abc123".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_guard_skips_when_no_entries() {
        let dir = std::env::temp_dir().join("chud_repo_test_cleanup_guard");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("keep_me.zip"), b"data").unwrap();

        cleanup_removed(&dir, &HashSet::new(), false);
        assert!(dir.join("keep_me.zip").exists(), "empty-list guard must not wipe existing files");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_removes_files_absent_from_expected_set() {
        let dir = std::env::temp_dir().join("chud_repo_test_cleanup_removes");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stale.zip"), b"data").unwrap();
        std::fs::write(dir.join("fresh.zip"), b"data").unwrap();

        let mut expected = HashSet::new();
        expected.insert("fresh.zip".to_string());
        cleanup_removed(&dir, &expected, true);

        assert!(!dir.join("stale.zip").exists());
        assert!(dir.join("fresh.zip").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
