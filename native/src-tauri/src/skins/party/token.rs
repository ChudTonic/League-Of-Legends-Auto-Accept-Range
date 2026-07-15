//! Party token codec (S6) — ported from `party/protocol/token_codec.py`.
//!
//! Wire layout is byte-exact with the Python original (and therefore with
//! any peer still holding a Python-issued token): before compression, v2 is
//! `>BIQ` (version: u8, timestamp: u32 big-endian, summoner_id: u64
//! big-endian) followed by the 32-byte room secret — 45 bytes total —
//! zlib-compressed and urlsafe-base64 encoded with padding stripped. v1
//! (legacy P2P) inserts a 2x u16 ip/port pair between summoner_id and the
//! secret; decode still accepts it for back-compat, encode never produces it.
//!
//! IMPORTANT — the 32-byte `room_secret` is NOT an encryption key and NOTHING
//! in this codebase encrypts party payloads with it. Its ONLY job is to make
//! the relay room name unguessable: `relay::compute_room_key` does
//! `sha256(host_id + room_secret)[:32]`, so a token holder and the host
//! independently derive the same private room address. Party frames travel to
//! the relay as plaintext JSON (protected only by TLS in transit and, as of
//! P0-F, carrying an ephemeral id + display name + cosmetic skin pick — no
//! summoner id); their integrity is guaranteed by ed25519 signatures
//! (`party::sig`), not by any cipher here. See `docs/PRIVACY-PARTY.md`.
//!
//! The ONLY branded surface is the ASCII `"CHUD:"` prefix
//! (`docs/SKINS_PORT.md` §1); the binary layout matches the upstream codec, so
//! a token minted by either side decodes on the other once the prefix is
//! stripped.

#![allow(dead_code)]

use std::io::{Read, Write};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;

pub const TOKEN_PREFIX: &str = "CHUD:";
pub const TOKEN_VERSION: u8 = 2;
pub const TOKEN_EXPIRY_SECONDS: u64 = 3600;

/// Decoded party token contents (ported from `PartyToken`'s fields; `encode`/
/// `decode`/`is_expired` are free functions here instead of methods since
/// this module has no class to hang them off of).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenData {
    pub version: u8,
    pub timestamp: u32,
    /// Historically a real LCU summoner id. As of P0-F, `party::manager`'s
    /// `enable()` instead puts a random per-`enable()` EPHEMERAL id here
    /// (see its doc comment) — this module's wire layout and field name are
    /// unchanged, only the caller-supplied value's meaning is.
    pub summoner_id: u64,
    /// 32-byte room-derivation secret — hashed into the relay room name, never
    /// used to encrypt anything (see this module's doc comment).
    pub room_secret: [u8; 32],
}

impl TokenData {
    /// `PartyToken.is_expired` — `now_unix` is the caller's
    /// `SystemTime::now()` unix-seconds snapshot (kept a parameter so this
    /// stays deterministic/testable rather than sampling the clock itself).
    pub fn is_expired(&self, now_unix: u64) -> bool {
        now_unix > self.timestamp as u64 + TOKEN_EXPIRY_SECONDS
    }
}

#[derive(Debug)]
pub enum TokenError {
    Base64(base64::DecodeError),
    Decompress(std::io::Error),
    TooShort,
    UnsupportedVersion(u8),
    Expired,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::Base64(e) => write!(f, "token decoding failed: {e}"),
            TokenError::Decompress(e) => write!(f, "token decompression failed: {e}"),
            TokenError::TooShort => write!(f, "token data too short"),
            TokenError::UnsupportedVersion(v) => write!(f, "unsupported token version: {v}"),
            TokenError::Expired => write!(f, "token has expired"),
        }
    }
}

impl std::error::Error for TokenError {}

/// `PartyToken.encode` — `timestamp` is the token creation time (Unix
/// seconds); the caller passes `SystemTime::now()` (see this module's doc
/// comment on why the clock read isn't inlined here).
pub fn encode_token(summoner_id: u64, room_secret: &[u8; 32], timestamp: u32) -> String {
    let mut data = Vec::with_capacity(13 + 32);
    data.push(TOKEN_VERSION);
    data.extend_from_slice(&timestamp.to_be_bytes());
    data.extend_from_slice(&summoner_id.to_be_bytes());
    data.extend_from_slice(room_secret);

    // zlib level 9, matching `zlib.compress(data, level=9)` — the compressed
    // bytes need not match Python byte-for-byte (different zlib builds can
    // emit different deflate streams for the same input/level); what matters
    // for wire-compat is that `decode_token` on either side can inflate
    // whatever the other side produced, which any standard zlib stream
    // guarantees.
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(9));
    encoder.write_all(&data).expect("in-memory zlib write cannot fail");
    let compressed = encoder.finish().expect("in-memory zlib finish cannot fail");

    let encoded = URL_SAFE_NO_PAD.encode(compressed);
    format!("{TOKEN_PREFIX}{encoded}")
}

/// `PartyToken.decode` — accepts both v1 (legacy P2P, IP/port fields kept
/// only so an old token still decodes) and v2 (relay-only) layouts.
/// `now_unix` is the caller's current unix-seconds snapshot, checked against
/// the decoded token's expiry.
pub fn decode_token(token_str: &str, now_unix: u64) -> Result<TokenData, TokenError> {
    let stripped = token_str.strip_prefix(TOKEN_PREFIX).unwrap_or(token_str);

    let compressed = URL_SAFE_NO_PAD.decode(stripped).map_err(TokenError::Base64)?;
    let mut data = Vec::new();
    ZlibDecoder::new(&compressed[..]).read_to_end(&mut data).map_err(TokenError::Decompress)?;

    if data.len() < 13 {
        return Err(TokenError::TooShort);
    }
    let version = data[0];

    let (timestamp, summoner_id, room_secret) = match version {
        1 => {
            // Legacy v1: >BIQHH (version, timestamp, summoner_id, ip, port) + 32-byte secret.
            if data.len() < 57 {
                return Err(TokenError::TooShort);
            }
            let timestamp = u32::from_be_bytes(data[1..5].try_into().unwrap());
            let summoner_id = u64::from_be_bytes(data[5..13].try_into().unwrap());
            let mut room_secret = [0u8; 32];
            room_secret.copy_from_slice(&data[25..57]);
            (timestamp, summoner_id, room_secret)
        }
        2 => {
            // v2: >BIQ (version, timestamp, summoner_id) + 32-byte secret.
            if data.len() < 45 {
                return Err(TokenError::TooShort);
            }
            let timestamp = u32::from_be_bytes(data[1..5].try_into().unwrap());
            let summoner_id = u64::from_be_bytes(data[5..13].try_into().unwrap());
            let mut room_secret = [0u8; 32];
            room_secret.copy_from_slice(&data[13..45]);
            (timestamp, summoner_id, room_secret)
        }
        other => return Err(TokenError::UnsupportedVersion(other)),
    };

    let token = TokenData { version, timestamp, summoner_id, room_secret };
    if token.is_expired(now_unix) {
        return Err(TokenError::Expired);
    }
    Ok(token)
}

/// Generate a fresh 32-byte room secret (ported from `create_token`'s
/// `secrets.token_bytes(32)`). Used only to derive an unguessable relay room
/// name — NOT an encryption key (see this module's doc comment).
pub fn generate_room_secret() -> [u8; 32] {
    use rand::RngCore;
    let mut secret = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut secret);
    secret
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trips() {
        let room_secret = [7u8; 32];
        let token = encode_token(123456789, &room_secret, 1_700_000_000);
        assert!(token.starts_with(TOKEN_PREFIX));

        let decoded = decode_token(&token, 1_700_000_100).expect("decode should succeed");
        assert_eq!(decoded.version, TOKEN_VERSION);
        assert_eq!(decoded.timestamp, 1_700_000_000);
        assert_eq!(decoded.summoner_id, 123456789);
        assert_eq!(decoded.room_secret, room_secret);
    }

    #[test]
    fn decode_rejects_expired_token() {
        let room_secret = [1u8; 32];
        let token = encode_token(1, &room_secret, 1_000_000_000);
        // now_unix far past timestamp + TOKEN_EXPIRY_SECONDS.
        let err = decode_token(&token, 1_000_000_000 + TOKEN_EXPIRY_SECONDS + 1).unwrap_err();
        assert!(matches!(err, TokenError::Expired));
    }

    #[test]
    fn decode_accepts_token_without_prefix() {
        let room_secret = [2u8; 32];
        let token = encode_token(42, &room_secret, 1_700_000_000);
        let bare = token.strip_prefix(TOKEN_PREFIX).unwrap();
        let decoded = decode_token(bare, 1_700_000_000).unwrap();
        assert_eq!(decoded.summoner_id, 42);
    }

    /// Known-vector cross-check: this exact string was produced by the
    /// Python `token_codec.py` (with the prefix swapped to `CHUD:`) using
    /// `struct.pack(">BIQ", 2, 1700000000, 123456789) + bytes(range(32))`,
    /// `zlib.compress(level=9)`, `base64.urlsafe_b64encode` — proving this
    /// port decodes a Python-issued token byte-for-byte, not just its own
    /// output.
    #[test]
    fn decodes_known_python_issued_v2_vector() {
        let token = "CHUD:eNpjSg3-yAAC7NFnRRkYmZhZWNnYOTi5uHl4-fgFBIWERUTFxCUkpaRlZOXkAYgKBOA";
        let decoded = decode_token(token, 1_700_000_100).expect("known vector should decode");
        assert_eq!(decoded.version, 2);
        assert_eq!(decoded.timestamp, 1_700_000_000);
        assert_eq!(decoded.summoner_id, 123456789);
        let expected_key: Vec<u8> = (0u8..32).collect();
        assert_eq!(decoded.room_secret.to_vec(), expected_key);
    }

    /// Same cross-check for the legacy v1 layout — note `token_codec.py`'s
    /// decode only `struct.unpack(">BIQHH", data[:17])`s the header, then
    /// reads the key from `data[25:57]`, leaving an 8-byte gap (`[17:25]`)
    /// it never parses; this vector reproduces that gap (zero-filled) so it
    /// matches what the Python decoder actually expects on the wire (no v1
    /// encoder exists anymore in either codebase — decode-only back-compat).
    #[test]
    fn decodes_known_python_issued_v1_vector() {
        let token = "CHUD:eNpjTA3-yAAC7NFnRRmQASMTMwsrGzsHJxc3Dy8fv4CgkLCIqJi4hKSUtIysnDwAqxEE3w";
        let decoded = decode_token(token, 1_700_000_100).expect("known v1 vector should decode");
        assert_eq!(decoded.version, 1);
        assert_eq!(decoded.timestamp, 1_700_000_000);
        assert_eq!(decoded.summoner_id, 123456789);
        let expected_key: Vec<u8> = (0u8..32).collect();
        assert_eq!(decoded.room_secret.to_vec(), expected_key);
    }
}
