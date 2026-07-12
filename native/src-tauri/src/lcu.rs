//! League Client (LCU) access: lockfile auth discovery + a small reqwest client.
//!
//! Auth comes from the LCU **lockfile** in the League install directory
//! (`<install>/lockfile`, format `LeagueClient:pid:port:password:https`). The
//! install dir is found from the running `LeagueClientUx.exe` process. The LCU
//! serves a self-signed cert on 127.0.0.1; we accept invalid certs (scoped to
//! loopback). TODO(hardening): pin Riot's `riotgames.pem` instead.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use reqwest::header::AUTHORIZATION;
use sysinfo::{ProcessesToUpdate, System};

#[derive(Debug, Clone)]
pub struct Auth {
    pub base_url: String,
    /// Pre-built `Basic <base64>` Authorization header value.
    pub header: String,
}

fn install_dir_from_cmd(cmd: &[OsString]) -> Option<PathBuf> {
    for arg in cmd {
        let s = arg.to_string_lossy();
        if let Some(rest) = s.strip_prefix("--install-directory=") {
            return Some(PathBuf::from(rest.trim_matches('"')));
        }
    }
    None
}

fn read_lockfile(path: &Path) -> Option<Auth> {
    let text = std::fs::read_to_string(path).ok()?;
    let parts: Vec<&str> = text.trim().split(':').collect();
    if parts.len() < 5 {
        return None;
    }
    let port = parts[2];
    let password = parts[3];
    let token = STANDARD.encode(format!("riot:{password}"));
    Some(Auth {
        base_url: format!("https://127.0.0.1:{port}"),
        header: format!("Basic {token}"),
    })
}

/// Locate the running League client and read its lockfile. Returns `None` when
/// the client isn't running or the lockfile can't be read yet.
pub fn find_auth() -> Option<Auth> {
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    for proc in sys.processes().values() {
        if proc.name().to_string_lossy().to_lowercase() != "leagueclientux.exe" {
            continue;
        }
        let dir = proc
            .exe()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .or_else(|| install_dir_from_cmd(proc.cmd()));
        if let Some(dir) = dir {
            if let Some(auth) = read_lockfile(&dir.join("lockfile")) {
                return Some(auth);
            }
        }
    }
    None
}

// Cached auth so per-image asset requests don't rescan the process list each
// time. The lockfile port/password rotate per client launch, so callers
// invalidate on a failed request to force a fresh discovery.
static AUTH_CACHE: std::sync::Mutex<Option<Auth>> = std::sync::Mutex::new(None);

pub fn cached_auth() -> Option<Auth> {
    if let Some(a) = AUTH_CACHE.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return Some(a);
    }
    let found = find_auth();
    *AUTH_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = found.clone();
    found
}

pub fn invalidate_auth() {
    *AUTH_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

pub fn build_client(timeout_secs: f64) -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true) // LCU self-signed cert on 127.0.0.1
        .timeout(Duration::from_secs_f64(timeout_secs.max(0.5)))
        .build()
        .expect("failed to build reqwest client")
}

/// Shared client for LCU asset proxying: one connection pool / TLS setup
/// reused across all `lcu://` scheme requests instead of a fresh client per
/// image (the Profile view can fetch 100+ icons in a burst).
pub fn asset_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| build_client(5.0))
}

pub async fn get_phase(client: &reqwest::Client, auth: &Auth) -> Option<String> {
    let resp = client
        .get(format!("{}/lol-gameflow/v1/gameflow-phase", auth.base_url))
        .header(AUTHORIZATION, &auth.header)
        .send()
        .await
        .ok()?;
    let text = resp.text().await.ok()?;
    Some(text.trim().trim_matches('"').to_string())
}

pub async fn accept_match(client: &reqwest::Client, auth: &Auth) -> bool {
    client
        .post(format!("{}/lol-matchmaking/v1/ready-check/accept", auth.base_url))
        .header(AUTHORIZATION, &auth.header)
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// Generic authed GET returning parsed JSON, or None on any failure.
pub async fn get_json(client: &reqwest::Client, auth: &Auth, path: &str) -> Option<serde_json::Value> {
    let resp = client
        .get(format!("{}{}", auth.base_url, path))
        .header(AUTHORIZATION, &auth.header)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

/// Authed write to the LCU with an optional JSON body — the POST/PUT/DELETE/
/// PATCH side that `get_json` doesn't cover (rune pages, item sets, champ-
/// select selection). Returns the parsed response body on 2xx (or `Null` for
/// an empty 2xx body, which many LCU writes return), and `None` on a transport
/// failure or non-2xx status.
pub async fn request_json(
    client: &reqwest::Client,
    auth: &Auth,
    method: reqwest::Method,
    path: &str,
    body: Option<&serde_json::Value>,
) -> Option<serde_json::Value> {
    let mut req = client.request(method, format!("{}{}", auth.base_url, path)).header(AUTHORIZATION, &auth.header);
    if let Some(b) = body {
        req = req.json(b);
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let text = resp.text().await.ok()?;
    if text.trim().is_empty() {
        Some(serde_json::Value::Null)
    } else {
        serde_json::from_str(&text).ok()
    }
}

/// Authed GET returning raw bytes + content-type — for proxying LCU asset
/// images (`/lol-game-data/assets/...`) to the WebView.
pub async fn get_bytes(client: &reqwest::Client, auth: &Auth, path: &str) -> Option<(Vec<u8>, String)> {
    let resp = client
        .get(format!("{}{}", auth.base_url, path))
        .header(AUTHORIZATION, &auth.header)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/png")
        .to_string();
    let bytes = resp.bytes().await.ok()?.to_vec();
    Some((bytes, content_type))
}

#[allow(dead_code)] // used by the ranked kill-switch in M2/M3
pub async fn gameflow_session(client: &reqwest::Client, auth: &Auth) -> Option<serde_json::Value> {
    let resp = client
        .get(format!("{}/lol-gameflow/v1/session", auth.base_url))
        .header(AUTHORIZATION, &auth.header)
        .send()
        .await
        .ok()?;
    resp.json().await.ok()
}
