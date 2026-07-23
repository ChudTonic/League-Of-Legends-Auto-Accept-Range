// Applies the saved theme to <html> BEFORE first paint, so there's no flash of
// the default palette. Must stay a separate self-hosted file (not inline): the
// strict Tauri CSP is script-src 'self', which blocks inline <script>. Loaded in
// <head> of BOTH index.html and overlay.html. localStorage is shared across
// Chud's webviews (same tauri://localhost origin), so both windows boot to the
// same theme; main.js reconciles against the durable Rust config right after.
try {
  document.documentElement.dataset.theme = localStorage.getItem("chud-theme") || "neon";
} catch (_) {
  document.documentElement.dataset.theme = "neon";
}
