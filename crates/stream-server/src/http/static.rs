//! Static file hosting for the compiled selkies-dashboard dist (design 2.2).
//!
//! Stage1 route: every non-API GET path serves a file under `--dist`. Index
//! fallback: a request ending in `/` resolves to `index.html`; a missing file
//! returns a 404 text body instead of falling back to the index (SPA history
//! fallback is not required because the client loads at the root).
//!
//! Directory traversal: query and fragment are stripped first; path segments
//! are normalized over `dist`; any `..` that would escape the root rejects the
//! request. MIME is a fixed extension table — no `mime_guess` dependency, which
//! keeps the WS stack on the single tungstenite copy shared with hbb_common.
//!
//! #62 caching: the dashboard bundle is served over a fresh TLS connection per
//! request (`Connection: close`), so a cold first paint used to read every asset
//! from disk again. `CACHE` keeps the resolved bytes (as `Arc`) keyed by the
//! normalized path, revalidated against mtime + length so an edited dist is
//! picked up without a restart.
//!
//! #99 validators: `CACHE` only saves the server the read. Without an
//! entity-tag the browser had nothing to revalidate against, so `no-cache`
//! behaved like `no-store` and every refresh re-downloaded the whole bundle.
//! Each response now carries the file's SHA-256 from
//! `dist/.asset-manifest.json` as a strong `ETag`, and content-hashed paths
//! under `assets/` are marked `immutable`. Together with the 304 path in
//! `server`, a reload costs a few hundred bytes instead of ~1.3 MB — and the
//! manifest is what makes that hash trustworthy rather than invented.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use crate::http::manifest;

/// A static response body ready to write back on the socket. The payload is
/// shared with the cache, so a hit costs one `Arc` clone instead of a file read.
#[derive(Debug, Clone)]
pub struct StaticFile {
    pub mime: String,
    pub bytes: Arc<Vec<u8>>,
    /// Strong validator derived from the bundle's hash table. `None` when the
    /// dist has no manifest row for this file (see `policy_for`).
    pub etag: Option<String>,
    /// Value for the `Cache-Control` response header.
    pub cache_control: &'static str,
}

/// Browser revalidates on every use; the 304 is cheap and always correct.
const CACHE_REVALIDATE: &str = "no-cache";
/// Content-hashed URL, so the bytes behind it can never change.
const CACHE_IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// Upper bound on cached entries. The bundle is ~2000 files once `jsdb/` is
/// included, and a client that scrolls a gamepad-heavy page touches hundreds of
/// them; 256 made the cache thrash and re-read from disk mid-session.
const CACHE_MAX_ENTRIES: usize = 4096;
/// Upper bound on cached bytes. Entering a single page can pull in most of the
/// bundle, so the ceiling is a byte budget rather than a count: once it is
/// crossed the cache is dropped whole, which costs one re-read each and keeps
/// the implementation free of an LRU list.
const CACHE_MAX_BYTES: u64 = 32 * 1024 * 1024;

struct CachedFile {
    mime: String,
    bytes: Arc<Vec<u8>>,
    modified: Option<SystemTime>,
    len: u64,
}

static CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedFile>>> = OnceLock::new();
/// Running total of `CACHE` payload bytes, kept beside the map so the budget
/// check does not walk 2000 entries on every insert.
static CACHE_BYTES: AtomicU64 = AtomicU64::new(0);

fn cache() -> &'static Mutex<HashMap<PathBuf, CachedFile>> {
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Fixed extension -> MIME table for the dashboard bundle (JS, CSS, SVG,
/// images, fonts, wasm, source maps).
fn mime_for(ext: &str) -> &'static str {
    match ext {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "map" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "webp" => "image/webp",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "wasm" => "application/wasm",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Strip query and fragment, then normalize segments against `dist`. Returns
/// None when the path escapes the root or decodes to a directory.
fn resolve_path(dist: &Path, url_path: &str) -> Option<PathBuf> {
    let clean = url_path.split(['?', '#']).next().unwrap_or("");
    let clean = clean.strip_prefix('/').unwrap_or(clean);

    let mut root = dist.to_path_buf();
    for seg in clean.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if !root.pop() {
                    return None;
                }
            }
            name => root.push(name),
        }
    }

    // After normalization the joined path must still live under dist; an escaped
    // root returns None rather than serving outside the static root.
    if !root.starts_with(dist) {
        return None;
    }
    Some(root)
}

/// Validators for one file, taken from the bundle's hash table.
///
/// A file the table does not know (a dist built before #99, or anything dropped
/// into `dist` by hand) gets `no-cache` and no tag: still correct, just not
/// incremental. `immutable` is only ever claimed for paths the build confirmed
/// carry a content hash in their name — never guessed from the extension.
fn policy_for(dist: &Path, candidate: &Path) -> (Option<String>, &'static str) {
    let rel = candidate.strip_prefix(dist).ok().and_then(|p| p.to_str());
    let policy = match rel {
        Some(rel) => manifest::policy(dist, rel),
        None => manifest::Policy::default(),
    };

    (
        policy.etag,
        if policy.immutable { CACHE_IMMUTABLE } else { CACHE_REVALIDATE },
    )
}

/// Read `dist/url_path` as a static file; `Some(Ok(_))` on hit, `Some(Err(_))`
/// on a resolved-but-unreadable file, `None` when the path does not exist.
pub fn serve(dist: &Path, url_path: &str) -> io::Result<Option<StaticFile>> {
    let Some(mut candidate) = resolve_path(dist, url_path) else {
        return Ok(None);
    };

    // Directory or bare request -> index.html fallback.
    if candidate.is_dir() || candidate.as_os_str().is_empty() {
        candidate = dist.join("index.html");
    }

    let meta = match std::fs::metadata(&candidate) {
        Ok(meta) => meta,
        Err(_) => return Ok(None),
    };
    if meta.is_dir() {
        return Ok(None);
    }
    let modified = meta.modified().ok();
    let len = meta.len();

    // Computed before the cache lookup so a hit and a miss describe the file
    // identically; the table is itself cached, so this is a hash-map lookup.
    let (etag, cache_control) = policy_for(dist, &candidate);

    // Cache hit: same path, same size, same mtime -> serve the shared bytes.
    if let Ok(guard) = cache().lock() {
        if let Some(hit) = guard.get(&candidate) {
            if hit.len == len && hit.modified == modified {
                return Ok(Some(StaticFile {
                    mime: hit.mime.clone(),
                    bytes: hit.bytes.clone(),
                    etag,
                    cache_control,
                }));
            }
        }
    }

    let bytes = std::fs::read(&candidate)?;
    let ext = candidate
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let mime = mime_for(&ext).to_owned();
    let bytes = Arc::new(bytes);
    if let Ok(mut guard) = cache().lock() {
        if guard.len() >= CACHE_MAX_ENTRIES
            || CACHE_BYTES.load(Ordering::Relaxed) + len > CACHE_MAX_BYTES
        {
            guard.clear();
            CACHE_BYTES.store(0, Ordering::Relaxed);
        }
        if let Some(old) = guard.insert(
            candidate,
            CachedFile {
                mime: mime.clone(),
                bytes: bytes.clone(),
                modified,
                len,
            },
        ) {
            CACHE_BYTES.fetch_sub(old.len, Ordering::Relaxed);
        }
        CACHE_BYTES.fetch_add(len, Ordering::Relaxed);
    }
    Ok(Some(StaticFile { mime, bytes, etag, cache_control }))
}

/// 404 body for an unresolved static path.
pub fn not_found() -> StaticFile {
    StaticFile {
        mime: "text/plain; charset=utf-8".to_owned(),
        bytes: Arc::new(b"404 Not Found".to_vec()),
        etag: None,
        cache_control: CACHE_REVALIDATE,
    }
}
