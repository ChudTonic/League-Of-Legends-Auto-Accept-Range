//! `modscan-core` — a defensive scanner for League of Legends mod archives
//! (`.fantome` files, which are just renamed `.zip`s).
//!
//! Intentionally PURE: no Tauri, no networking, no filesystem writes — it only
//! reads the bytes it's handed and reports findings. Shared between the Chud
//! app and a standalone `modscan` CLI, and trivial to unit test/fuzz in isolation.
//!
//! `scan_bytes` is the only entry point: hand it a file's bytes, get a
//! `ScanReport`. Never panics, never does unbounded work — every read is capped
//! (see `MAX_*` below), so a hostile .fantome can't DoS the scanner itself.

use std::collections::BTreeMap;
use std::io::Read;

use serde::Serialize;
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Zip bombs and "make the reviewer's tool choke" archives love absurd entry
/// counts. Past this we already know the answer (Malicious) — no need to
/// trust the entry count enough to allocate per-entry state for all of them.
pub const MAX_ENTRIES: usize = 50_000;

/// Sum of every entry's *declared* uncompressed size. A few KB compressed
/// can claim to expand to terabytes (classic zip bomb) — this is the
/// "would filesystem-fill the machine on naive extract" tripwire.
pub const MAX_TOTAL_UNCOMPRESSED: u64 = 4 * 1024 * 1024 * 1024; // 4 GiB

/// Same idea as `MAX_TOTAL_UNCOMPRESSED` but for a single member — one
/// enormous entry can trip this even if the archive as a whole looks small.
pub const MAX_SINGLE_ENTRY_UNCOMPRESSED: u64 = 2 * 1024 * 1024 * 1024; // 2 GiB

/// Uncompressed/compressed ratio past which an entry is "too good to be real
/// cosmetic data" (DEFLATE tops ~1000:1 on degenerate input like zero-runs;
/// legit textures/wads never approach 200:1). Entries under 1 KiB compressed
/// are ignored — tiny files hit silly ratios without being a bomb.
pub const MAX_COMPRESSION_RATIO: u64 = 200;

/// How many leading bytes of an entry's *decompressed* stream we sniff for a
/// magic number. Bounded deliberately — we never want to decompress a whole
/// entry just to classify it.
pub const MAGIC_SNIFF_BYTES: usize = 16;

/// Cap on how many `unexpected-content` findings we emit for one archive.
/// A mod pack with a thousand stray files shouldn't turn into a thousand
/// findings — group by extension and stop after this many groups.
const MAX_UNEXPECTED_CONTENT_FINDINGS: usize = 20;

/// Extensions that are simply never legitimate in a cosmetic skin mod.
/// Anything here is a runnable/loadable payload on Windows regardless of
/// what the archive author *called* it.
const DANGEROUS_EXTENSIONS: &[&str] = &[
    "exe", "dll", "sys", "scr", "com", "bat", "cmd", "ps1", "psm1", "vbs", "vbe", "js", "jse",
    "wsf", "wsh", "hta", "jar", "msi", "msp", "lnk", "url", "reg", "cpl", "gadget", "inf", "pif",
    "application", "msc", "ocx", "drv", "efi",
];

/// Extensions a normal League cosmetic mod (WAD overlay, texture swap, VFX
/// tweak, etc.) is expected to contain. Anything else is `unexpected-content`
/// — not proof of malice by itself, but off-contract and worth a human look.
const COSMETIC_EXTENSIONS: &[&str] = &[
    "wad", "client", "dds", "tex", "png", "jpg", "jpeg", "tga", "skn", "skl", "scb", "sco", "anm",
    "mapgeo", "bin", "troybin", "wpk", "bnk", "wem", "ogg", "preload", "luaobj", "json", "txt",
    "subchunktoc", "dat",
];

/// Archive/compression extensions — a nested archive can hide payloads from
/// a shallow (non-recursive) scan of the outer .fantome.
const NESTED_ARCHIVE_EXTENSIONS: &[&str] =
    &["zip", "fantome", "rar", "7z", "gz", "bz2", "xz", "tar", "cab"];

/// Windows reserved device names. A path component matching one of these
/// (extension stripped, case-insensitive) can misbehave badly on extraction
/// (e.g. `WAD/CON.dds` tries to open the CON device, not a file).
const RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Overall call on an archive. Anything `Malicious` should block install;
/// `Suspicious` is a "warn the user, let them decide" tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Clean,
    Suspicious,
    Malicious,
}

/// Severity of a single finding. Kept separate from `Verdict` — a report can
/// carry Info findings alongside its final verdict without changing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Suspicious,
    Malicious,
}

/// A single detection. `entry` is the archive-relative path the finding is
/// about, when it's about one specific entry (grouped findings, or
/// whole-archive findings like `too-many-entries`, leave it `None`).
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub severity: Severity,
    pub code: String,
    pub entry: Option<String>,
    pub detail: String,
}

impl Finding {
    fn new(severity: Severity, code: &str, entry: Option<String>, detail: impl Into<String>) -> Self {
        Finding { severity, code: code.to_string(), entry, detail: detail.into() }
    }
}

/// Full result of scanning one archive.
#[derive(Debug, Clone, Serialize)]
pub struct ScanReport {
    pub verdict: Verdict,
    /// Hex-encoded SHA-256 of the whole input file, regardless of whether it
    /// parsed as a zip — this is how a caller correlates a report back to a
    /// specific file even if the "not a zip" path was taken.
    pub sha256: String,
    pub file_size: u64,
    pub entry_count: usize,
    pub total_uncompressed: u64,
    pub findings: Vec<Finding>,
}

impl ScanReport {
    /// True when this archive should be blocked outright (install refused),
    /// as opposed to `Suspicious`, which is a warn-and-let-the-user-decide.
    pub fn is_blocking(&self) -> bool {
        self.verdict == Verdict::Malicious
    }

    /// Multi-line, human-readable rendering of the report — used by the CLI
    /// in non-`--json` mode and handy in logs/error messages from callers.
    pub fn human_summary(&self) -> String {
        let mut out = format!(
            "verdict: {:?}\nsha256: {}\nsize: {} bytes\nentries: {}\ntotal uncompressed: {} bytes\n",
            self.verdict, self.sha256, self.file_size, self.entry_count, self.total_uncompressed
        );
        if self.findings.is_empty() {
            out.push_str("findings: none\n");
        } else {
            out.push_str("findings:\n");
            for f in &self.findings {
                let entry = f.entry.as_deref().unwrap_or("-");
                out.push_str(&format!("  [{:?}] {} ({}): {}\n", f.severity, f.code, entry, f.detail));
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Scan a whole `.fantome`/`.zip` file's bytes and return a report. Never
/// panics: any per-entry failure (corrupt entry, decompression error) is
/// downgraded to an `unreadable-entry` Info finding rather than propagated.
pub fn scan_bytes(data: &[u8]) -> ScanReport {
    let sha256 = hex_sha256(data);
    let file_size = data.len() as u64;

    let mut archive = match zip::ZipArchive::new(std::io::Cursor::new(data)) {
        Ok(archive) => archive,
        Err(_) => {
            // A .fantome that isn't even a valid zip is off-contract, but
            // that alone isn't evidence of malware — could just be a
            // corrupt download. Flag it and move on; sha256 is still useful
            // for correlating this report to the file.
            return ScanReport {
                verdict: Verdict::Suspicious,
                sha256,
                file_size,
                entry_count: 0,
                total_uncompressed: 0,
                findings: vec![Finding::new(
                    Severity::Info,
                    "not-a-zip",
                    None,
                    "file could not be opened as a zip archive",
                )],
            };
        }
    };

    let mut findings = Vec::new();
    let entry_count = archive.len();

    if entry_count == 0 {
        findings.push(Finding::new(Severity::Suspicious, "empty-archive", None, "archive contains no entries"));
    }

    if entry_count > MAX_ENTRIES {
        findings.push(Finding::new(
            Severity::Malicious,
            "too-many-entries",
            None,
            format!("archive has {entry_count} entries, exceeding the {MAX_ENTRIES} guardrail"),
        ));
    }

    // Bound how many entries we actually iterate — an absurd entry count is
    // already a verdict on its own; we don't need to pay for per-entry work
    // on all of them to prove it further.
    let scan_limit = entry_count.min(MAX_ENTRIES);

    let mut total_uncompressed: u64 = 0;
    let mut total_uncompressed_exceeded = false;

    // Grouped by extension so a pack with hundreds of stray files produces
    // a handful of findings, not one per file.
    let mut unexpected_content: BTreeMap<String, Vec<String>> = BTreeMap::new();

    let mut has_meta = false;
    let mut has_wad_member = false;
    let mut has_wad_dir = false;

    for i in 0..scan_limit {
        let mut entry = match archive.by_index(i) {
            Ok(entry) => entry,
            Err(err) => {
                findings.push(Finding::new(
                    Severity::Info,
                    "unreadable-entry",
                    None,
                    format!("entry {i} could not be read: {err}"),
                ));
                continue;
            }
        };

        let raw_name = entry.name().to_string();
        let normalized = raw_name.replace('\\', "/");
        let is_dir = entry.is_dir();

        // --- 1. Path safety (zip-slip, absolute paths, ADS, device names) ---
        let traversal_reason = path_traversal_reason(&normalized);
        let enclosed_rejected = entry.enclosed_name().is_none();
        let is_traversal = traversal_reason.is_some() || enclosed_rejected;
        if let Some(reason) = &traversal_reason {
            findings.push(Finding::new(Severity::Malicious, "path-traversal", Some(raw_name.clone()), reason.clone()));
        } else if enclosed_rejected {
            findings.push(Finding::new(
                Severity::Malicious,
                "path-traversal",
                Some(raw_name.clone()),
                "enclosed_name() rejected this path as unsafe",
            ));
        }

        // --- 2. Symlink entries (mode bits, unix_mode is None on non-unix zips) ---
        if let Some(mode) = entry.unix_mode() {
            if mode & 0o170000 == 0o120000 {
                findings.push(Finding::new(
                    Severity::Malicious,
                    "symlink",
                    Some(raw_name.clone()),
                    "entry is a symlink — can point outside the extraction root",
                ));
            }
        }

        let ext = extension_of(&normalized);

        // --- 3. Dangerous extension ---
        let dangerous_ext = ext.as_deref().is_some_and(|e| DANGEROUS_EXTENSIONS.contains(&e));
        if dangerous_ext {
            findings.push(Finding::new(
                Severity::Malicious,
                "dangerous-extension",
                Some(raw_name.clone()),
                format!("extension \"{}\" is a runnable/loadable payload type, never legitimate in a cosmetic mod", ext.as_deref().unwrap_or("")),
            ));
        }

        // --- 4. Magic-byte disguise (bounded read; never for directories) ---
        let mut disguised = false;
        if !is_dir {
            let mut buf = Vec::with_capacity(MAGIC_SNIFF_BYTES);
            // `take` bounds the underlying decompression to MAGIC_SNIFF_BYTES
            // — we never decompress a whole entry just to sniff its header.
            let mut limited = (&mut entry).take(MAGIC_SNIFF_BYTES as u64);
            match limited.read_to_end(&mut buf) {
                Ok(_) => {
                    if let Some(kind) = sniff_executable_magic(&buf) {
                        disguised = true;
                        findings.push(Finding::new(
                            Severity::Malicious,
                            "disguised-executable",
                            Some(raw_name.clone()),
                            format!("entry content sniffs as {kind}, regardless of its \"{}\" extension", ext.as_deref().unwrap_or("(none)")),
                        ));
                    }
                    if !disguised && sniff_nested_archive_magic(&buf) {
                        findings.push(Finding::new(
                            Severity::Suspicious,
                            "nested-archive",
                            Some(raw_name.clone()),
                            "entry content sniffs as a nested archive header",
                        ));
                    }
                }
                Err(err) => {
                    findings.push(Finding::new(
                        Severity::Info,
                        "unreadable-entry",
                        Some(raw_name.clone()),
                        format!("could not read entry data for magic sniff: {err}"),
                    ));
                }
            }
        }

        // --- 5. Compression-ratio bomb ---
        let size = entry.size();
        let compressed_size = entry.compressed_size();
        let ratio_bomb = (compressed_size >= 1024 && compressed_size > 0 && size / compressed_size > MAX_COMPRESSION_RATIO)
            || size > MAX_SINGLE_ENTRY_UNCOMPRESSED;
        if ratio_bomb {
            findings.push(Finding::new(
                Severity::Suspicious,
                "compression-bomb-entry",
                Some(raw_name.clone()),
                format!(
                    "uncompressed size {size} bytes from {compressed_size} bytes compressed \
                     — decompression-bomb ratio or absolute size guardrail tripped"
                ),
            ));
        }

        // --- 6. Nested archive by extension (magic-based hit already logged above) ---
        let nested_by_ext = ext.as_deref().is_some_and(|e| NESTED_ARCHIVE_EXTENSIONS.contains(&e));
        if nested_by_ext {
            findings.push(Finding::new(
                Severity::Suspicious,
                "nested-archive",
                Some(raw_name.clone()),
                "nested-archive extension can hide payloads from a shallow scan",
            ));
        }

        // --- 7. Content-type classification (skip dirs and anything already flagged malicious) ---
        if !is_dir && !is_traversal && !dangerous_ext && !disguised {
            let is_cosmetic = match ext.as_deref() {
                Some(e) => COSMETIC_EXTENSIONS.contains(&e),
                // No extension is only fine for hashed WAD members (e.g.
                // `WAD/3AB2...`); a bare-name file anywhere else is unusual.
                None => path_has_component(&normalized, "wad"),
            };
            if !is_cosmetic {
                let key = ext.clone().unwrap_or_else(|| "(no extension)".to_string());
                unexpected_content.entry(key).or_default().push(raw_name.clone());
            }
        }

        // --- running total for the zip-bomb-total guardrail ---
        if !total_uncompressed_exceeded {
            total_uncompressed = total_uncompressed.saturating_add(size);
            if total_uncompressed > MAX_TOTAL_UNCOMPRESSED {
                total_uncompressed_exceeded = true;
                findings.push(Finding::new(
                    Severity::Suspicious,
                    "zip-bomb-total",
                    None,
                    format!("running total of declared uncompressed sizes exceeded {MAX_TOTAL_UNCOMPRESSED} bytes"),
                ));
            }
        }

        // --- structure sanity bookkeeping ---
        if path_has_component(&normalized, "meta") {
            has_meta = true;
        }
        if path_has_component(&normalized, "wad") {
            has_wad_dir = true;
        }
        if ext.as_deref() == Some("wad") || normalized.to_lowercase().ends_with(".wad.client") {
            has_wad_member = true;
        }
    }

    // --- 7 (cont'd). Emit the grouped unexpected-content findings, capped ---
    let total_groups = unexpected_content.len();
    for (ext, entries) in unexpected_content.into_iter().take(MAX_UNEXPECTED_CONTENT_FINDINGS) {
        let mut example_entries = entries.clone();
        example_entries.truncate(5);
        let suffix = if entries.len() > example_entries.len() { ", ..." } else { "" };
        findings.push(Finding::new(
            Severity::Suspicious,
            "unexpected-content",
            None,
            format!(
                "{} entr{} with extension \"{ext}\" not in the cosmetic-mod allowlist: {}{suffix}",
                entries.len(),
                if entries.len() == 1 { "y" } else { "ies" },
                example_entries.join(", "),
            ),
        ));
    }
    if total_groups > MAX_UNEXPECTED_CONTENT_FINDINGS {
        findings.push(Finding::new(
            Severity::Suspicious,
            "unexpected-content",
            None,
            format!("{} more unexpected extensions omitted for brevity", total_groups - MAX_UNEXPECTED_CONTENT_FINDINGS),
        ));
    }

    // --- 8. Structure sanity ---
    if !has_meta && !has_wad_member && !has_wad_dir {
        findings.push(Finding::new(
            Severity::Info,
            "no-mod-structure",
            None,
            "no META/ entry, WAD/ directory, or .wad(.client) member found — doesn't look like a normal mod",
        ));
    }

    let verdict = if findings.iter().any(|f| f.severity == Severity::Malicious) {
        Verdict::Malicious
    } else if findings.iter().any(|f| f.severity == Severity::Suspicious) {
        Verdict::Suspicious
    } else {
        Verdict::Clean
    };

    ScanReport { verdict, sha256, file_size, entry_count, total_uncompressed, findings }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn hex_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Returns `Some(reason)` if `normalized` (already `\`→`/` normalized) is
/// unsafe to extract, covering the zip-slip / absolute-path / ADS / device
/// name / trailing-space-or-dot tricks. Order matters only for which
/// `detail` string a caller sees — any hit is Malicious regardless.
fn path_traversal_reason(normalized: &str) -> Option<String> {
    if normalized.split('/').any(|c| c == "..") {
        return Some("path contains a \"..\" component (zip-slip)".to_string());
    }
    if normalized.starts_with('/') {
        return Some("absolute path".to_string());
    }
    let bytes = normalized.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Some("Windows drive-letter prefix".to_string());
    }
    if normalized.contains(':') {
        return Some("contains ':' — possible NTFS alternate data stream".to_string());
    }
    for component in normalized.split('/').filter(|c| !c.is_empty()) {
        let base = component.split('.').next().unwrap_or(component);
        if RESERVED_NAMES.contains(&base.to_uppercase().as_str()) {
            return Some(format!("path component \"{component}\" is a reserved Windows device name"));
        }
        if component.ends_with(' ') || component.ends_with('.') {
            return Some(format!(
                "path component \"{component}\" has a trailing space/dot (Windows path-normalization trick)"
            ));
        }
    }
    None
}

/// Lowercased final extension of a (already `/`-normalized) archive path, or
/// `None` if the final path component has no extension.
fn extension_of(normalized: &str) -> Option<String> {
    let basename = normalized.rsplit('/').next().unwrap_or(normalized);
    std::path::Path::new(basename).extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase())
}

/// True if any path component (case-insensitive) equals `name`, e.g.
/// `path_has_component("WAD/Foo.wad.client", "wad")` is true.
fn path_has_component(normalized: &str, name: &str) -> bool {
    normalized.split('/').any(|c| c.eq_ignore_ascii_case(name))
}

/// Sniff `buf` (up to `MAGIC_SNIFF_BYTES` of an entry's decompressed head)
/// for known executable/script magic numbers. This is the check that
/// catches a payload renamed to look cosmetic (e.g. `Splash.dds` that is
/// actually a Windows PE).
fn sniff_executable_magic(buf: &[u8]) -> Option<&'static str> {
    if buf.starts_with(&[0x4D, 0x5A]) {
        return Some("a PE (MZ) executable");
    }
    if buf.starts_with(&[0x7F, 0x45, 0x4C, 0x46]) {
        return Some("an ELF executable");
    }
    if buf.starts_with(&[0xFE, 0xED, 0xFA, 0xCE])
        || buf.starts_with(&[0xFE, 0xED, 0xFA, 0xCF])
        || buf.starts_with(&[0xCA, 0xFE, 0xBA, 0xBE])
        || buf.starts_with(&[0xCF, 0xFA, 0xED, 0xFE])
        || buf.starts_with(&[0xCE, 0xFA, 0xED, 0xFE])
    {
        return Some("a Mach-O executable");
    }
    if buf.starts_with(&[0x23, 0x21]) {
        return Some("a script with a shebang (#!)");
    }
    if buf.starts_with(&[0x4C, 0x00, 0x00, 0x00]) {
        return Some("a Windows shortcut (.lnk header)");
    }
    None
}

/// Sniff `buf` for a nested-archive magic number, independent of the outer
/// entry's extension (catches an archive renamed to look like cosmetic data).
fn sniff_nested_archive_magic(buf: &[u8]) -> bool {
    buf.starts_with(&[0x50, 0x4B, 0x03, 0x04]) // PK\x03\x04
        || buf.starts_with(&[0x52, 0x61, 0x72, 0x21, 0x1A]) // Rar!\x1a
        || buf.starts_with(&[0x37, 0x7A, 0xBC, 0xAF]) // 7z\xbc\xaf
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    /// Build a `.fantome`-shaped zip in memory from `(name, bytes)` pairs,
    /// using STORE (no compression) so ratio-based tests get predictable
    /// compressed sizes.
    fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write as _;
        let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default();
        for (name, data) in entries {
            writer.start_file(*name, opts).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn clean_cosmetic_mod_is_clean() {
        let bytes = build_zip(&[
            ("META/info.json", br#"{"Name":"Test Skin"}"#),
            ("WAD/Foo.wad.client", b"fake wad bytes"),
            ("WAD/texture.dds", b"fake dds bytes"),
        ]);
        let report = scan_bytes(&bytes);
        assert_eq!(report.verdict, Verdict::Clean, "{:?}", report.findings);
        assert!(!report.sha256.is_empty());
    }

    #[test]
    fn zip_slip_is_malicious() {
        let bytes = build_zip(&[("../../evil.txt", b"pwned")]);
        let report = scan_bytes(&bytes);
        assert_eq!(report.verdict, Verdict::Malicious);
        assert!(report.findings.iter().any(|f| f.code == "path-traversal"));
    }

    #[test]
    fn absolute_and_drive_and_ads_paths_are_malicious() {
        for name in ["/etc/passwd", "C:\\x", "foo.dds:bad"] {
            let bytes = build_zip(&[(name, b"data")]);
            let report = scan_bytes(&bytes);
            assert_eq!(report.verdict, Verdict::Malicious, "name={name} findings={:?}", report.findings);
            assert!(
                report.findings.iter().any(|f| f.code == "path-traversal"),
                "name={name} findings={:?}",
                report.findings
            );
        }
    }

    #[test]
    fn disguised_executable_is_malicious() {
        let mut payload = vec![b'M', b'Z'];
        payload.extend_from_slice(&[0u8; 32]);
        let bytes = build_zip(&[("WAD/Splash.dds", &payload)]);
        let report = scan_bytes(&bytes);
        assert_eq!(report.verdict, Verdict::Malicious);
        assert!(report.findings.iter().any(|f| f.code == "disguised-executable"));
    }

    #[test]
    fn dangerous_extension_is_malicious() {
        let bytes = build_zip(&[("helper.exe", b"MZ fake pe")]);
        let report = scan_bytes(&bytes);
        assert_eq!(report.verdict, Verdict::Malicious);
        assert!(report.findings.iter().any(|f| f.code == "dangerous-extension"));
    }

    #[test]
    fn lua_entry_is_suspicious_unexpected_content() {
        let bytes = build_zip(&[("scripts/hook.lua", b"print('hi')")]);
        let report = scan_bytes(&bytes);
        assert_eq!(report.verdict, Verdict::Suspicious);
        assert!(report.findings.iter().any(|f| f.code == "unexpected-content"));
    }

    #[test]
    fn nested_archive_is_suspicious() {
        let inner = build_zip(&[("a.txt", b"hi")]);
        let bytes = build_zip(&[("inner.zip", &inner)]);
        let report = scan_bytes(&bytes);
        assert_eq!(report.verdict, Verdict::Suspicious);
        assert!(report.findings.iter().any(|f| f.code == "nested-archive"));
    }

    #[test]
    fn compression_bomb_entry_is_suspicious() {
        // STORE doesn't compress at all, so DEFLATE is needed here for a real ratio.
        let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        writer.start_file("WAD/zeros.bin", opts).unwrap();
        let zeros = vec![0u8; 5 * 1024 * 1024];
        std::io::Write::write_all(&mut writer, &zeros).unwrap();
        let bytes = writer.finish().unwrap().into_inner();

        let report = scan_bytes(&bytes);
        assert_eq!(report.verdict, Verdict::Suspicious, "{:?}", report.findings);
        assert!(report.findings.iter().any(|f| f.code == "compression-bomb-entry"));
    }

    #[test]
    fn reserved_device_name_is_malicious() {
        let bytes = build_zip(&[("WAD/CON.dds", b"data")]);
        let report = scan_bytes(&bytes);
        assert_eq!(report.verdict, Verdict::Malicious);
        assert!(report.findings.iter().any(|f| f.code == "path-traversal"));
    }

    #[test]
    fn not_a_zip_is_suspicious_but_still_hashed() {
        let bytes = b"this is definitely not a zip file".to_vec();
        let report = scan_bytes(&bytes);
        assert_eq!(report.verdict, Verdict::Suspicious);
        assert!(report.findings.iter().any(|f| f.code == "not-a-zip"));
        assert_eq!(report.sha256.len(), 64);
    }

    #[test]
    fn sha256_is_correct_and_stable() {
        // sha256("hello") — a well-known test vector.
        let report = scan_bytes(b"hello");
        assert_eq!(report.sha256, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
    }
}
