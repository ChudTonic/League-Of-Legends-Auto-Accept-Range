//! Referral program client — OPT-IN ONLY. Talks to the chud-referral worker
//! (see referral-worker/SPEC.md). Nothing here ever runs for a non-participant:
//! `mint` fires only when the user clicks "Refer a friend", `claim` only when
//! they enter a friend's code, `activate` only when an install_code already
//! exists. Deliberately separate from the anonymous telemetry channel — the only
//! machine identifier ever sent is a salted hash, and only for participants.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const BASE: &str = "https://chud-referral.jivy26.workers.dev";
// Client-side salt so the raw MachineGuid never leaves the machine (the worker
// re-salts again with its own secret).
const HWID_APP_SALT: &str = "chud-referral-hwid-v1";

/// Windows MachineGuid (HKLM\SOFTWARE\Microsoft\Cryptography) — a stable
/// per-install id. `None` if unreadable (non-Windows or locked-down registry).
#[cfg(windows)]
fn machine_guid() -> Option<String> {
    use windows::core::w;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let mut buf = [0u16; 128];
    let mut size = (buf.len() * std::mem::size_of::<u16>()) as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\Cryptography"),
            w!("MachineGuid"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    if rc.0 != 0 {
        return None;
    }
    let chars = (size as usize / std::mem::size_of::<u16>()).saturating_sub(1); // drop trailing NUL
    Some(String::from_utf16_lossy(&buf[..chars]).trim().to_string())
}
#[cfg(not(windows))]
fn machine_guid() -> Option<String> {
    None
}

fn hwid_hash() -> String {
    let guid = machine_guid().unwrap_or_else(|| "unknown-machine".to_string());
    let mut h = Sha256::new();
    h.update(HWID_APP_SALT.as_bytes());
    h.update(b"|");
    h.update(guid.as_bytes());
    format!("{:x}", h.finalize())
}

fn client() -> reqwest::Client {
    crate::net::build_external_client(12.0, crate::net::built_in_allowed_origins())
}

/// Mint (or fetch the existing) referrer code for this machine.
pub async fn mint() -> Result<Value, String> {
    let res = client()
        .post(format!("{BASE}/refer/mint"))
        .json(&json!({ "hwid_hash": hwid_hash() }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    res.json::<Value>().await.map_err(|e| e.to_string())
}

/// Claim a friend's referral code → returns `{install_code}`. Errors surface the
/// worker's generic "code not accepted" (unknown code / machine already claimed).
pub async fn claim(ref_code: &str) -> Result<Value, String> {
    let res = client()
        .post(format!("{BASE}/refer/claim"))
        .json(&json!({ "ref_code": ref_code, "hwid_hash": hwid_hash() }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let v: Value = res.json().await.map_err(|e| e.to_string())?;
    if v.get("install_code").and_then(Value::as_str).is_some() {
        Ok(v)
    } else {
        Err(v.get("error").and_then(Value::as_str).unwrap_or("code not accepted").to_string())
    }
}

/// Fire-and-forget activation ping. Safe to call repeatedly (worker is
/// idempotent); `maybe_activate` also flips a config flag so it only fires once.
pub fn activate(install_code: String) {
    tauri::async_runtime::spawn(async move {
        let _ = client()
            .post(format!("{BASE}/refer/activate"))
            .json(&json!({ "install_code": install_code }))
            .send()
            .await;
    });
}

/// Called after a successful skin injection. If this install was referred and
/// hasn't activated yet, ping the worker once and record it. No-op for
/// non-participants (no install_code) — nothing ever leaves the machine.
pub fn maybe_activate(app: &tauri::AppHandle) {
    use crate::LockExt;
    use tauri::Manager;
    let state = app.state::<std::sync::Arc<crate::AppState>>();
    let (install_code, activated) = {
        let c = state.config.lock_safe();
        (c.referral.install_code.clone(), c.referral.activated)
    };
    if install_code.is_empty() || activated {
        return;
    }
    activate(install_code);
    let mut c = state.config.lock_safe();
    c.referral.activated = true;
    let _ = c.save();
}
