//! The bundle's hash table (`dist/.asset-manifest.json`), read by the static
//! server to answer "what is this file, exactly".
//!
//! Why the server needs it at all: `http::static` used to answer every request
//! with `Cache-Control: no-cache` and no validator. That is correct (a rebuilt
//! dist is picked up without a restart) but it means a browser re-downloads the
//! ~1.3 MB of JS/CSS on every single refresh — nothing to revalidate against,
//! so `no-cache` degrades into `no-store`. The table is generated at build time
//! and carries a real SHA-256 per file, which lets the server emit a strong
//! `ETag` and a 304 when the bytes have not changed. That, plus Vite's content
//! hashes in `assets/`, is what makes the refresh incremental.
//!
//! The file is optional. A dist built before this module existed has no table;
//! `lookup` returns `None`, the caller falls back to `no-cache` with no
//! validator, and behaviour is exactly what it was.
//!
//! Reload policy mirrors `static`: the parsed table is kept in memory and
//! revalidated against the file's mtime + length, so a redeployed dist is
//! picked up on the next request instead of needing a server restart.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

/// File name of the table inside the `--dist` root. Kept dot-prefixed so it
/// cannot be confused with the PWA `manifest.json` at the same level.
pub const FILE_NAME: &str = ".asset-manifest.json";

/// One row of the table: a file the build listed individually.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Entry {
    /// Byte length as recorded at build time.
    #[serde(default)]
    pub size: u64,
    /// Lowercase hex SHA-256 of the file's bytes.
    #[serde(default)]
    pub sha256: String,
    /// `immutable` for content-hashed paths, `revalidate` for everything else.
    #[serde(default)]
    pub cache: String,
}

/// A group of files hashed together — the gamepad DB, one aggregate per
/// platform, because gendb rewrites all of it from a single upstream snapshot.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Unit {
    /// How many files the aggregate covers.
    #[serde(default)]
    pub files: u64,
    /// Their total size.
    #[serde(default)]
    pub bytes: u64,
    /// SHA-256 over `<path>\0<sha256>\n` for every member, in path order.
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub cache: String,
}

/// What the static server needs to know about one path.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    /// Entity-tag, already quoted. `None` means "no validator".
    pub etag: Option<String>,
    /// Whether the response may be cached without revalidation.
    pub immutable: bool,
}

impl Entry {
    /// The HTTP entity-tag for this file, or `None` when the row carries no
    /// hash (a hand-written table, or a schema we do not understand).
    ///
    /// Strong, not weak: the hash is over the exact bytes served, so a range
    /// request or a proxy can compare it byte-for-byte.
    pub fn etag(&self) -> Option<String> {
        if self.sha256.is_empty() {
            return None;
        }
        Some(format!("\"sha256-{}\"", self.sha256))
    }

    /// Whether responses for this file may be cached without revalidation.
    pub fn immutable(&self) -> bool {
        self.cache == "immutable"
    }
}

/// The subset of the table the server reads. Unknown fields are ignored so a
/// newer build can add rows for the deploy chain without breaking old servers.
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct Table {
    #[serde(default)]
    files: HashMap<String, Entry>,
    #[serde(default)]
    units: HashMap<String, Unit>,
}

struct Loaded {
    /// Which dist this table belongs to. A redeploy can switch `--dist` at the
    /// same time as it rewrites the table, and mtime alone would not tell the
    /// two apart.
    path: std::path::PathBuf,
    modified: Option<SystemTime>,
    len: u64,
    raw: Arc<Vec<u8>>,
    table: Table,
    /// Entity-tag for the table itself, so a client polling `/api/v1/manifest`
    /// revalidates instead of re-reading it.
    etag: String,
}

static CACHE: OnceLock<Mutex<Option<Arc<Loaded>>>> = OnceLock::new();

fn cache() -> &'static Mutex<Option<Arc<Loaded>>> {
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Load (or reuse) the table for `dist`. `None` when the bundle predates it or
/// the file is not valid JSON — both mean "no validators available", not an
/// error worth failing a request over.
fn load(dist: &Path) -> Option<Arc<Loaded>> {
    let path = dist.join(FILE_NAME);
    let meta = std::fs::metadata(&path).ok()?;
    let modified = meta.modified().ok();
    let len = meta.len();

    if let Ok(guard) = cache().lock() {
        if let Some(hit) = guard.as_ref() {
            if hit.path == path && hit.len == len && hit.modified == modified {
                return Some(hit.clone());
            }
        }
    }

    let raw = Arc::new(std::fs::read(&path).ok()?);
    let table: Table = match serde_json::from_slice(&raw) {
        Ok(table) => table,
        Err(err) => {
            log::warn!("{} is not a usable manifest: {err}", path.display());
            return None;
        }
    };

    // Weak tag derived from the file's identity: the table is regenerated on
    // every build, so mtime + length moves whenever it matters. Avoiding a
    // second hash pass keeps the cold path at one read.
    let stamp = modified
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let etag = format!("W/\"manifest-{len}-{stamp}\"");

    let loaded = Arc::new(Loaded { path: path.clone(), modified, len, raw, table, etag });
    if let Ok(mut guard) = cache().lock() {
        *guard = Some(loaded.clone());
    }
    log::info!(
        "asset manifest loaded: {} file row(s) from {}",
        loaded.table.files.len(),
        path.display()
    );
    Some(loaded)
}

/// Look up one path relative to the dist root (`assets/index-abc.js`). Both
/// separators are normalized to `/`, which is what the table uses.
pub fn lookup(dist: &Path, rel: &str) -> Option<Entry> {
    let loaded = load(dist)?;
    let key = rel.trim_start_matches('/').replace('\\', "/");
    loaded.table.files.get(&key).cloned()
}

/// The longest unit key that is a directory ancestor of `rel`.
///
/// Units are not enumerated by prefix in the server: the build decides the
/// grouping, and this matches whatever it produced. `/` after the key matters —
/// otherwise `jsdb/windows-extra/x.json` would be attributed to `jsdb/windows`.
fn unit_key<'a>(units: &'a HashMap<String, Unit>, rel: &str) -> Option<&'a str> {
    units
        .keys()
        .filter(|key| rel.len() > key.len() + 1
            && rel.starts_with(key.as_str())
            && rel.as_bytes()[key.len()] == b'/')
        .max_by_key(|key| key.len())
        .map(|key| key.as_str())
}

/// Validators for `rel`.
///
/// A file listed in `files` uses its own hash. A file inside a unit uses the
/// unit's aggregate hash *plus its path*: the aggregate is computed over every
/// `path\0hash` pair, so an unchanged aggregate proves every member unchanged,
/// and the path keeps two members from sharing one tag. What this cannot do is
/// tell you *which* member changed — irrelevant here, since the question the
/// server answers is only "is your copy of this file still current".
pub fn policy(dist: &Path, rel: &str) -> Policy {
    let Some(loaded) = load(dist) else {
        return Policy::default();
    };
    let key = rel.trim_start_matches('/').replace('\\', "/");

    if let Some(entry) = loaded.table.files.get(&key) {
        return Policy { etag: entry.etag(), immutable: entry.immutable() };
    }

    if let Some(unit) = unit_key(&loaded.table.units, &key).and_then(|k| loaded.table.units.get(k)) {
        if unit.sha256.is_empty() {
            return Policy::default();
        }
        return Policy {
            etag: Some(format!("\"{}-{}\"", unit.sha256, key)),
            immutable: unit.cache == "immutable",
        };
    }

    Policy::default()
}

/// The raw table and its entity-tag, for `GET /api/v1/manifest`.
pub fn body(dist: &Path) -> Option<(Arc<Vec<u8>>, String)> {
    let loaded = load(dist)?;
    Some((loaded.raw.clone(), loaded.etag.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_table(dir: &Path, json: &str) {
        std::fs::write(dir.join(FILE_NAME), json).unwrap();
    }

    #[test]
    fn etag_is_strong_and_quoted() {
        let entry = Entry {
            size: 3,
            sha256: "abc123".to_owned(),
            cache: "immutable".to_owned(),
        };
        assert_eq!(entry.etag().as_deref(), Some("\"sha256-abc123\""));
        assert!(entry.immutable());
    }

    #[test]
    fn entry_without_hash_has_no_etag() {
        let entry = Entry { size: 0, sha256: String::new(), cache: String::new() };
        assert_eq!(entry.etag(), None);
        assert!(!entry.immutable());
    }

    #[test]
    fn lookup_ignores_the_leading_slash_and_missing_files() {
        let dir = std::env::temp_dir().join(format!("cs-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_table(
            &dir,
            r#"{"schema":1,"files":{"index.html":{"size":9,"sha256":"ff00","cache":"revalidate"}}}"#,
        );

        let hit = lookup(&dir, "/index.html").expect("index.html is in the table");
        assert_eq!(hit.sha256, "ff00");
        assert!(lookup(&dir, "assets/missing.js").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_table_is_not_an_error() {
        let dir = std::env::temp_dir().join(format!("cs-manifest-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(lookup(&dir, "index.html").is_none());
        assert!(body(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_inside_a_unit_borrows_the_aggregate_tag() {
        let dir = std::env::temp_dir().join(format!("cs-manifest-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_table(
            &dir,
            r#"{"schema":1,
                "files":{"index.html":{"size":9,"sha256":"ff00","cache":"revalidate"}},
                "units":{"jsdb/windows":{"files":868,"bytes":100,"sha256":"aabb","cache":"revalidate"}}}"#,
        );

        // Inside the unit: the aggregate tag plus the path, so members differ.
        let inner = policy(&dir, "/jsdb/windows/045e-028e.json");
        assert_eq!(inner.etag.as_deref(), Some("\"aabb-jsdb/windows/045e-028e.json\""));
        assert!(!inner.immutable);

        // A sibling directory that merely shares the prefix must not inherit it.
        assert!(policy(&dir, "jsdb/windows-extra/x.json").etag.is_none());
        // The unit key itself is not a file.
        assert!(policy(&dir, "jsdb/windows").etag.is_none());
        // A per-file row still wins and carries its own tag.
        assert_eq!(policy(&dir, "index.html").etag.as_deref(), Some("\"sha256-ff00\""));
        // Anything the table does not mention gets no validator.
        assert!(policy(&dir, "assets/unknown.js").etag.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
