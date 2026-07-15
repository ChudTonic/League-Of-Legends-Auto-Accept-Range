//! Party selection signing (P0-F) — pure ed25519 sign/verify helpers factored
//! out of `manager.rs` so the crypto logic is unit-testable without a live
//! relay connection or LCU. A selection's signature binds it to
//! `(epoch, member_id, champion_id, skin_id, chroma_id, custom_mod_hash,
//! announcer_mod_id)` so a captured payload can't be replayed into a
//! different room instance (the relay mints a fresh `epoch` per DO instance)
//! or reattributed to a different `member_id` — the relay itself doesn't
//! enforce any of this, it just relays whatever `sanitize_skin` (in
//! `relay-worker`) accepts, so verification is entirely on the client.

#![allow(dead_code)]

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

/// Hex-encode, matching the lowercase `{:02x}` convention every other hex
/// field in this codebase uses (room keys, mod-content hashes, ...).
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decode a hex string into bytes; `None` on odd length or a non-hex digit.
pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// The exact byte string a selection's signature is computed over — every
/// field pinned in a fixed order and `|`-joined so there's no ambiguity
/// between, say, `skin_id=12,chroma_id=3` and `skin_id=123,chroma_id=<none>`.
fn signing_payload(
    epoch: &str,
    member_id: u64,
    champion_id: i64,
    skin_id: i64,
    chroma: i64,
    hash: &str,
    announcer: &str,
) -> String {
    format!("{epoch}|{member_id}|{champion_id}|{skin_id}|{chroma}|{hash}|{announcer}")
}

/// Sign a selection with our ephemeral per-`enable()` session key.
/// `chroma` is `-1` for "no chroma", `hash`/`announcer` are `"-"` when unset
/// (matching `manager.rs::broadcast_skin_update`'s field conventions).
pub fn sign_selection(
    key: &SigningKey,
    epoch: &str,
    member_id: u64,
    champion_id: i64,
    skin_id: i64,
    chroma: i64,
    hash: &str,
    announcer: &str,
) -> String {
    let payload = signing_payload(epoch, member_id, champion_id, skin_id, chroma, hash, announcer);
    let sig: Signature = key.sign(payload.as_bytes());
    to_hex(&sig.to_bytes())
}

/// Verify a peer's selection signature against their advertised pubkey.
/// `false` on anything malformed (bad hex, wrong lengths, bad signature) —
/// never partial-trust a selection that doesn't check out completely.
#[allow(clippy::too_many_arguments)]
pub fn verify_selection(
    pubkey_hex: &str,
    epoch: &str,
    member_id: u64,
    champion_id: i64,
    skin_id: i64,
    chroma: i64,
    hash: &str,
    announcer: &str,
    sig_hex: &str,
) -> bool {
    let Some(pk_bytes) = from_hex(pubkey_hex) else { return false };
    let Ok(pk_arr): Result<[u8; 32], _> = pk_bytes.try_into() else { return false };
    let Ok(verifying) = VerifyingKey::from_bytes(&pk_arr) else { return false };

    let Some(sig_bytes) = from_hex(sig_hex) else { return false };
    let Ok(sig_arr): Result<[u8; 64], _> = sig_bytes.try_into() else { return false };
    let sig = Signature::from_bytes(&sig_arr);

    let payload = signing_payload(epoch, member_id, champion_id, skin_id, chroma, hash, announcer);
    verifying.verify_strict(payload.as_bytes(), &sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    fn key() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    #[test]
    fn valid_signature_verifies() {
        let k = key();
        let pubkey = to_hex(&k.verifying_key().to_bytes());
        let sig = sign_selection(&k, "epoch1", 42, 103, 103000, -1, "-", "-");
        assert!(verify_selection(&pubkey, "epoch1", 42, 103, 103000, -1, "-", "-", &sig));
    }

    #[test]
    fn wrong_member_id_fails() {
        let k = key();
        let pubkey = to_hex(&k.verifying_key().to_bytes());
        let sig = sign_selection(&k, "epoch1", 42, 103, 103000, -1, "-", "-");
        assert!(!verify_selection(&pubkey, "epoch1", 43, 103, 103000, -1, "-", "-", &sig));
    }

    #[test]
    fn wrong_epoch_fails() {
        let k = key();
        let pubkey = to_hex(&k.verifying_key().to_bytes());
        let sig = sign_selection(&k, "epoch1", 42, 103, 103000, -1, "-", "-");
        assert!(!verify_selection(&pubkey, "epoch2", 42, 103, 103000, -1, "-", "-", &sig));
    }

    #[test]
    fn tampered_skin_id_fails() {
        let k = key();
        let pubkey = to_hex(&k.verifying_key().to_bytes());
        let sig = sign_selection(&k, "epoch1", 42, 103, 103000, -1, "-", "-");
        assert!(!verify_selection(&pubkey, "epoch1", 42, 103, 999999, -1, "-", "-", &sig));
    }

    #[test]
    fn garbage_pubkey_fails() {
        let sig = "00".repeat(64);
        assert!(!verify_selection("not-hex", "epoch1", 42, 103, 103000, -1, "-", "-", &sig));
        assert!(!verify_selection(&"ab".repeat(32), "epoch1", 42, 103, 103000, -1, "-", "-", &sig));
    }
}
