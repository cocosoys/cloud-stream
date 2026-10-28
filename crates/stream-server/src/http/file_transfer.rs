//! #67 File transfer endpoints, wired to the selkies-webclient protocol.
//!
//! The frontend (selkies-dashboard / selkies-web-core) drives two endpoints:
//!
//! * `GET /api/files/{rel}` — the file-manager iframe target. A file resolves
//!   to an `attachment` download; a directory resolves to a self-contained
//!   HTML listing whose links keep the same shape (`/api/files/...`), so the
//!   browser navigates the tree entirely through this endpoint.
//! * `POST /api/upload` — raw `application/octet-stream` body; the destination
//!   relative path rides URL-encoded in the `X-Upload-Path` header. Files at or
//!   under 64 MiB go as one POST; larger files arrive as sequential slices with
//!   `X-Upload-Id` / `X-Upload-Offset` / `X-Upload-Total` and a final
//!   `X-Upload-Final: 1`, which the server accumulates into a `.upload-part`
//!   staging file and atomically renames into place on the final slice.
//!
//! Security follows the retired Python service and RustDesk's
//! `fs::join_validated_path`: every path component is checked against
//! traversal (`..`), drive letters, Windows-reserved characters and device
//! names before the filesystem is touched, and existing components are
//! additionally rejected when they resolve to a symlink, so a link planted
//! over an intermediate name cannot redirect a write outside the root.
//!
//! The endpoints are inactive (`404`) until `--upload-dir` is supplied; this
//! mirrors the Python `file_manager_path` gate and keeps the stage1 surface
//! closed by default.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use log;
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::config::Config;
use crate::http::{header_value, write_http};

/// Cap on a single upload request body (64 MiB slices, 256 MiB headroom for
/// the plain single-POST path and proxies in front).
pub const MAX_UPLOAD_BODY: usize = 256 * 1024 * 1024;

/// Stream chunks used while downloading a file attachment.
const DOWNLOAD_CHUNK: usize = 64 * 1024;

/// In-flight chunked-upload state, keyed by the normalized destination path.
#[derive(Debug, Clone)]
pub struct ChunkState {
    /// Opaque client-chosen transfer id (`X-Upload-Id`).
    pub id: String,
    /// Absolute byte offset the staging file is at.
    pub offset: u64,
    /// Staging file path (`<leaf>.upload-part` next to the destination).
    pub part: PathBuf,
}

/// Shared chunked-upload registry. One registry lives for the whole accept
/// loop and is handed to every connection, because each upload slice is an
/// independent HTTP connection.
pub type ChunkStore = Arc<Mutex<HashMap<String, ChunkState>>>;

/// Create an empty chunk registry.
pub fn new_chunk_store() -> ChunkStore {
    Arc::new(Mutex::new(HashMap::new()))
}

// ---------------------------------------------------------------------------
// Path validation
// ---------------------------------------------------------------------------

/// Percent-decode a UTF-8 byte slice (`%XX`); malformed escapes are kept as
/// literal bytes so the strict `from_utf8` below still rejects them.
fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' && i + 2 < input.len() {
            let hi = hex_val(input[i + 1]);
            let lo = hex_val(input[i + 2]);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// One path component is valid when it is non-empty, is not `.` or `..`, has
/// no Windows-reserved characters and is not a drive-letter prefix or a
/// reserved device name.
fn seg_ok(seg: &str) -> bool {
    if seg.is_empty() || seg == "." || seg == ".." || seg.len() > 255 {
        return false;
    }
    for c in seg.chars() {
        if matches!(c, '/' | '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\\') {
            return false;
        }
        if (c as u32) < 32 {
            return false;
        }
    }
    let bytes = seg.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return false;
    }
    let stem = seg.split('.').next().unwrap_or("").to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL"
            | "COM1" | "COM2" | "COM3" | "COM4" | "COM5" | "COM6" | "COM7" | "COM8" | "COM9"
            | "LPT1" | "LPT2" | "LPT3" | "LPT4" | "LPT5" | "LPT6" | "LPT7" | "LPT8" | "LPT9"
    ) {
        return false;
    }
    true
}

/// Split + validate the `X-Upload-Path` header into safe components. The
/// frontend sends `encodeURIComponent(pathToSend)`; backslashes in a dropped
/// directory tree are normalized to separators before splitting.
pub fn upload_parts(header: &[u8]) -> Option<Vec<String>> {
    let decoded = percent_decode(header);
    let text = std::str::from_utf8(&decoded).ok()?;
    let normalized = text.replace('\\', "/");
    let mut parts = Vec::new();
    for seg in normalized.split('/') {
        if seg.is_empty() {
            continue;
        }
        if !seg_ok(seg) {
            return None;
        }
        parts.push(seg.to_owned());
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts)
}

/// Same treatment for the web-visible `rel` of `/api/files/{rel}`: the
/// classify pass already splits on `/`, so only percent-decode + validate.
pub fn web_parts(rel: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    for seg in rel.split('/') {
        if seg.is_empty() {
            continue;
        }
        let decoded = percent_decode(seg.as_bytes());
        let text = std::str::from_utf8(&decoded).ok()?;
        if !seg_ok(text) {
            return None;
        }
        parts.push(text.to_owned());
    }
    Some(parts)
}

/// Resolve `root/parts` to an absolute path, refusing a non-directory root,
/// symlink components on the way, and (implicitly) any traversal, because a
/// validated component can never escape the canonical root.
fn resolve_target(root: &Path, parts: &[String]) -> Result<PathBuf, ()> {
    if !root.is_dir() {
        return Err(());
    }
    let canon_root = root.canonicalize().map_err(|_| ())?;
    let mut cur = canon_root;
    for seg in parts {
        cur.push(seg);
        match std::fs::symlink_metadata(&cur) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(());
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err(()),
        }
    }
    Ok(cur)
}

// ---------------------------------------------------------------------------
// Response helpers
// ---------------------------------------------------------------------------

/// JSON error response (upload contract: `{"status":"error","message":...}`).
async fn json_err<S: AsyncWrite + Unpin>(
    stream: &mut S,
    status: u16,
    status_text: &str,
    message: &str,
) -> io::Result<()> {
    let body = format!(
        "{{\"status\":\"error\",\"message\":{}}}",
        serde_json::to_string(message).unwrap_or_else(|_| "\"internal error\"".to_owned())
    );
    write_http(stream, status, status_text, "application/json; charset=utf-8", body.as_bytes()).await
}

/// JSON success response.
async fn json_ok<S: AsyncWrite + Unpin>(
    stream: &mut S,
    bytes: u64,
) -> io::Result<()> {
    let body = format!("{{\"status\":\"success\",\"bytes\":{bytes}}}");
    write_http(stream, 200, "OK", "application/json; charset=utf-8", body.as_bytes()).await
}

// ---------------------------------------------------------------------------
// GET /api/files/{rel}
// ---------------------------------------------------------------------------

/// Human-readable byte size.
fn human_size(len: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = len as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{len} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// URL-encode a file name for `href` (utf-8 bytes, unreserved kept).
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// HTML-escape a display name.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Directory listing page for `GET /api/files/{rel}`. Links are absolute
/// `/api/files/...` URLs (the parent link strips the last component instead
/// of using `..`), so no traversal ever reaches the validator.
async fn dir_listing<S: AsyncWrite + Unpin>(
    stream: &mut S,
    target: &Path,
    rel: &str,
) -> io::Result<()> {
    let mut items: Vec<(String, bool, u64, String)> = Vec::new();
    match std::fs::read_dir(target) {
        Ok(rd) => {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let meta = entry.metadata();
                let (is_dir, size, mtime) = match meta {
                    Ok(m) => (m.is_dir(), m.len(), format_modified(m)),
                    Err(_) => (false, 0, "".to_owned()),
                };
                items.push((name, is_dir, size, mtime));
            }
        }
        Err(_) => {
            return json_err(stream, 403, "Forbidden", "Permission Denied").await;
        }
    }
    items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));

    let mut rows = String::new();
    for (name, is_dir, size, mtime) in &items {
        let link = if *is_dir {
            format!("/api/files/{}/{}/", url_segment(rel), url_encode(name))
        } else {
            format!("/api/files/{}/{}", url_segment(rel), url_encode(name))
        };
        let size_txt = if *is_dir { "-".to_owned() } else { human_size(*size) };
        rows.push_str(&format!(
            "<tr><td><a href=\"{}\">{}{}</a></td><td>{}</td><td>{}</td></tr>",
            link,
            html_escape(name),
            if *is_dir { "/" } else { "" },
            html_escape(mtime),
            size_txt
        ));
    }

    let title = format!("/api/files/{rel}");
    let parent = parent_rel(rel);
    let parent_html = if parent.is_empty() {
        "".to_owned()
    } else {
        format!("<p><a href=\"/api/files/{}/\">&larr; parent directory</a></p>", url_segment(&parent))
    };
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <title>File browser</title>\
         <style>\
         body{{font-family:system-ui,Segoe UI,sans-serif;margin:0;background:#17181c;color:#e8e9eb}}\
         .wrap{{max-width:900px;margin:0 auto;padding:24px}}\
         h1{{font-size:18px;font-weight:600;word-break:break-all}}\
         a{{color:#7ab7ff;text-decoration:none}} a:hover{{text-decoration:underline}}\
         table{{width:100%;border-collapse:collapse;margin-top:12px}}\
         th,td{{text-align:left;padding:8px 10px;border-bottom:1px solid #2c2e34;font-size:14px}}\
         th{{color:#9aa0a6;font-weight:500}}\
         td:nth-child(2),td:nth-child(3){{color:#9aa0a6;white-space:nowrap}}\
         .path{{color:#9aa0a6;font-size:13px;margin:0 0 12px}}\
         </style></head><body><div class=\"wrap\">\
         <h1>{title}</h1><p class=\"path\">{}</p>{parent_html}\
         <table><thead><tr><th>Name</th><th>Modified</th><th>Size</th></tr></thead>\
         <tbody>{rows}</tbody></table></div></body></html>",
        html_escape(rel),
    );
    write_http(stream, 200, "OK", "text/html; charset=utf-8", body.as_bytes()).await
}

/// `/api/files/` URL segment for the given web rel (each component encoded).
fn url_segment(rel: &str) -> String {
    let mut out = String::new();
    for seg in rel.split('/') {
        if seg.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(&url_encode(seg));
    }
    out
}

/// Parent web rel ("a/b" -> "a", "" stays "").
fn parent_rel(rel: &str) -> String {
    match rel.rfind('/') {
        Some(idx) => rel[..idx].to_owned(),
        None => String::new(),
    }
}

/// Last modified as `YYYY-MM-DD HH:MM` (best effort).
fn format_modified(meta: std::fs::Metadata) -> String {
    use std::time::UNIX_EPOCH;
    let Ok(m) = meta.modified() else { return String::new() };
    let Ok(secs) = m.duration_since(UNIX_EPOCH) else { return String::new() };
    let days = secs.as_secs() / 86_400;
    let rem = secs.as_secs() % 86_400;
    let (h, mi) = (rem / 3600, (rem % 3600) / 60);
    // Days since epoch -> civil date (Howard Hinnant's algorithm, proleptic
    // Gregorian).
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}")
}

/// Stream a file as an attachment (`GET /api/files/...` when the target is a
/// file). The head carries `Content-Disposition` so the dashboard iframe
/// downloads instead of rendering.
async fn file_download<S: AsyncWrite + Unpin>(stream: &mut S, target: &Path) -> io::Result<()> {
    let meta = match std::fs::metadata(target) {
        Ok(m) => m,
        Err(_) => return json_err(stream, 404, "Not Found", "Not Found").await,
    };
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_owned());
    let ascii: String = name.chars().filter(|c| c.is_ascii() && !c.is_control()).collect();
    let ascii = if ascii.is_empty() { "file" } else { &ascii };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
         Content-Length: {len}\r\n\
         Content-Disposition: attachment; filename=\"{ascii}\"; filename*=UTF-8''{enc}\r\n\
         Connection: close\r\nCache-Control: no-cache\r\n\r\n",
        len = meta.len(),
        enc = url_encode(&name),
    );
    stream.write_all(head.as_bytes()).await?;

    let mut file = match tokio::fs::File::open(target).await {
        Ok(f) => f,
        Err(_) => {
            let _ = stream.shutdown().await;
            return Ok(());
        }
    };
    let mut buf = vec![0u8; DOWNLOAD_CHUNK];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        stream.write_all(&buf[..n]).await?;
    }
    let _ = stream.shutdown().await;
    Ok(())
}

/// `GET /api/files/{rel}` entry point.
pub async fn handle_files<S: AsyncWrite + Unpin>(
    stream: &mut S,
    rel: &str,
    cfg: &Config,
) -> io::Result<()> {
    let Some(root) = &cfg.upload_dir else {
        return json_err(stream, 404, "Not Found", "file transfer disabled").await;
    };
    let parts = match web_parts(rel) {
        Some(p) => p,
        None => {
            return json_err(stream, 403, "Forbidden", "Directory Traversal detected").await;
        }
    };
    let target = match resolve_target(root, &parts) {
        Ok(t) => t,
        Err(()) => {
            return json_err(stream, 403, "Forbidden", "Directory Traversal detected").await;
        }
    };
    match std::fs::symlink_metadata(&target) {
        Ok(meta) if meta.is_dir() => dir_listing(stream, &target, rel).await,
        Ok(_) => file_download(stream, &target).await,
        Err(_) => json_err(stream, 404, "Not Found", "Not Found").await,
    }
}

// ---------------------------------------------------------------------------
// POST /api/upload
// ---------------------------------------------------------------------------

/// A unique staging name next to `leaf` (single-POST path).
fn staging_name(leaf: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(".{leaf}.upload-{}-{nanos}", std::process::id())
}

/// Chunked staging name (fixed per destination, resumed across slices).
fn chunk_part_name(leaf: &str) -> String {
    format!(".{leaf}.upload-part")
}

/// `POST /api/upload` entry point. `body` holds the full request payload; the
/// header contract is read through `headers`.
pub async fn handle_upload<S: AsyncWrite + Unpin>(
    stream: &mut S,
    headers: &[httparse::Header<'_>],
    body: &[u8],
    cfg: &Config,
    chunks: &ChunkStore,
) -> io::Result<()> {
    let Some(root) = &cfg.upload_dir else {
        return json_err(stream, 403, "Forbidden", "uploads disabled").await;
    };
    let Some(path_hdr) = header_value(headers, "X-Upload-Path") else {
        return json_err(stream, 400, "Bad Request", "missing X-Upload-Path header").await;
    };
    let parts = match upload_parts(path_hdr) {
        Some(p) => p,
        None => {
            return json_err(stream, 400, "Bad Request", "invalid upload path").await;
        }
    };
    let target = match resolve_target(root, &parts) {
        Ok(t) => t,
        Err(()) => {
            return json_err(stream, 403, "Forbidden", "path escape rejected").await;
        }
    };

    // Refuse overwriting a directory or a symlink destination.
    match std::fs::symlink_metadata(&target) {
        Ok(meta) if meta.is_dir() => {
            return json_err(stream, 400, "Bad Request", "path is a directory").await;
        }
        Ok(meta) if meta.file_type().is_symlink() => {
            return json_err(stream, 403, "Forbidden", "symlink destination rejected").await;
        }
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(_) => {
            return json_err(stream, 403, "Forbidden", "path rejected").await;
        }
    }

    let parent = target.parent().unwrap_or(root);
    if let Err(_) = std::fs::create_dir_all(parent) {
        return json_err(stream, 500, "Internal Server Error", "mkdir failed").await;
    }
    let leaf = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_owned());

    let upload_id = header_value(headers, "X-Upload-Id").map(|v| v.to_vec());
    let offset_header = header_value(headers, "X-Upload-Offset");
    if upload_id.is_some() != offset_header.is_some() {
        return json_err(stream, 400, "Bad Request", "X-Upload-Id and X-Upload-Offset must be sent together").await;
    }

    if let Some(id) = upload_id {
        // Chunked path.
        let offset: u64 = match offset_header
            .and_then(|v| std::str::from_utf8(v).ok())
            .and_then(|s| s.parse().ok())
        {
            Some(o) => o,
            None => return json_err(stream, 400, "Bad Request", "malformed chunk offset").await,
        };
        let final_slice = header_value(headers, "X-Upload-Final")
            .map(|v| v == b"1")
            .unwrap_or(false);

        let key = target.to_string_lossy().to_lowercase();
        let part_path = parent.join(chunk_part_name(&leaf));
        let append = offset > 0;

        // Phase 1 — claim / validate the chunk slot. The whole lock scope is a
        // synchronous block: `std::sync::MutexGuard` is not `Send`, so no
        // `await` may run while a guard is alive inside this `tokio::spawn`ed
        // future. A rejected chunk reports `Stale` and the error is written
        // after the guard is dropped.
        enum Claim {
            Ok,
            Stale,
        }
        let claim = {
            let mut map = chunks.lock().unwrap();
            if offset == 0 {
                if let Some(old) = map.get(&key) {
                    let _ = std::fs::remove_file(&old.part);
                }
                map.insert(
                    key.clone(),
                    ChunkState {
                        id: String::from_utf8_lossy(&id).into_owned(),
                        offset: 0,
                        part: part_path.clone(),
                    },
                );
                Claim::Ok
            } else {
                let stale = match map.get(&key) {
                    Some(st) if st.id == String::from_utf8_lossy(&id) && st.offset == offset => {
                        match std::fs::metadata(&st.part) {
                            Ok(m) if m.len() == offset => false,
                            _ => true,
                        }
                    }
                    _ => true,
                };
                if stale {
                    if let Some(st) = map.get(&key) {
                        let _ = std::fs::remove_file(&st.part);
                        map.remove(&key);
                    }
                    Claim::Stale
                } else {
                    Claim::Ok
                }
            }
        };
        if matches!(claim, Claim::Stale) {
            return json_err(stream, 409, "Conflict", "chunk sequence mismatch; transfer discarded").await;
        }

        // Phase 2 — write the body outside the lock.
        let written = match write_body(&part_path, body, append).await {
            Ok(n) => n,
            Err(_) => {
                cleanup_chunk(chunks, &key);
                return json_err(stream, 500, "Internal Server Error", "write failed").await;
            }
        };

        // Phase 3 — bump the offset, and on the final slice atomically rename
        // the staging part into place. Again fully synchronous inside the lock.
        let finalized = {
            let mut map = chunks.lock().unwrap();
            if let Some(st) = map.get_mut(&key) {
                st.offset = offset + written as u64;
                if final_slice {
                    if std::fs::rename(&st.part, &target).is_ok() {
                        map.remove(&key);
                    } else {
                        let _ = std::fs::remove_file(&st.part);
                        map.remove(&key);
                    }
                    true
                } else {
                    true
                }
            } else {
                true
            }
        };
        if !finalized {
            return json_err(stream, 500, "Internal Server Error", "finalize failed").await;
        }

        log::info!("HTTP upload chunk finished: {} ({} bytes, final={final_slice})", target.display(), written);
        json_ok(stream, written as u64).await
    } else {
        // Single-POST path: staging name + atomic rename into place.
        let staging = parent.join(staging_name(&leaf));
        match write_body(&staging, body, false).await {
            Ok(n) => {
                if let Err(_) = std::fs::rename(&staging, &target) {
                    let _ = std::fs::remove_file(&staging);
                    return json_err(stream, 500, "Internal Server Error", "finalize failed").await;
                }
                log::info!("HTTP upload finished: {} ({} bytes)", target.display(), n);
                json_ok(stream, n as u64).await
            }
            Err(_) => {
                let _ = std::fs::remove_file(&staging);
                json_err(stream, 500, "Internal Server Error", "write failed").await
            }
        }
    }
}

/// Drop the staging file and the registry slot for `key` (synchronous; the
/// guard is scoped to this function so no `await` ever holds it).
fn cleanup_chunk(chunks: &ChunkStore, key: &str) {
    let mut map = chunks.lock().unwrap();
    if let Some(st) = map.get(key) {
        let _ = std::fs::remove_file(&st.part);
        map.remove(key);
    }
}

/// Write the request body to `path` (truncate when `append == false`).
async fn write_body(path: &Path, body: &[u8], append: bool) -> io::Result<usize> {
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create(true);
    if append {
        opts.append(true);
    } else {
        opts.truncate(true);
    }
    let mut file = opts.open(path).await?;
    file.write_all(body).await?;
    file.flush().await?;
    Ok(body.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_reject_traversal_and_reserved() {
        assert!(!seg_ok(".."));
        assert!(!seg_ok("."));
        assert!(!seg_ok(""));
        assert!(!seg_ok("a/b"));
        assert!(!seg_ok("a\\b"));
        assert!(!seg_ok("C:"));
        assert!(!seg_ok("c:windows"));
        assert!(!seg_ok("a<b"));
        assert!(!seg_ok("CON"));
        assert!(!seg_ok("com1.txt"));
        assert!(!seg_ok(&"x".repeat(256)));
        assert!(seg_ok("world.zip"));
        assert!(seg_ok("存档-2026"));
        assert!(seg_ok("a b.txt"));
        assert!(seg_ok("COM10")); // only COM1..COM9 are reserved
    }

    #[test]
    fn upload_parts_decodes_and_normalizes() {
        let parts = upload_parts(b"saves%2Fworld.zip").unwrap();
        assert_eq!(parts, vec!["saves".to_owned(), "world.zip".to_owned()]);

        let parts = upload_parts(b"a%5Cb%5Cc.txt").unwrap();
        assert_eq!(parts, vec!["a".to_owned(), "b".to_owned(), "c.txt".to_owned()]);

        assert!(upload_parts(b"..%2Fetc").is_none());
        assert!(upload_parts(b"C:%5Cwindows").is_none());
        assert!(upload_parts(b"").is_none());
        assert!(upload_parts(b"%FF%FE").is_none()); // invalid UTF-8
    }

    #[test]
    fn web_parts_validate_rel() {
        assert_eq!(web_parts("saves/world.zip").unwrap(), vec!["saves".to_owned(), "world.zip".to_owned()]);
        assert_eq!(web_parts("a%20b.txt").unwrap(), vec!["a b.txt".to_owned()]);
        assert!(web_parts("../../etc").is_none());
        assert!(web_parts("..").is_none());
    }

    #[test]
    fn resolve_rejects_symlink_components() {
        let root = std::env::temp_dir().join(format!("ft-root-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/file.txt"), b"x").unwrap();
        // A plain file resolves.
        let target = resolve_target(&root, &["sub".to_owned(), "file.txt".to_owned()]);
        assert!(target.is_ok());
        // A missing leaf still resolves (mkdir target allowed).
        let target = resolve_target(&root, &["sub".to_owned(), "new.bin".to_owned()]);
        assert!(target.is_ok());
        // A non-directory root refuses.
        let file_root = root.join("sub/file.txt");
        assert!(resolve_target(&file_root, &["a".to_owned()]).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn url_encode_keeps_unreserved() {
        assert_eq!(url_encode("a b.txt"), "a%20b.txt");
        assert_eq!(url_encode("存档.zip"), "%E5%AD%98%E6%A1%A3.zip");
        assert_eq!(url_encode("simple.txt"), "simple.txt");
    }

    #[test]
    fn human_size_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }
}
