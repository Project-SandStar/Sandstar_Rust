//! `/file/*path` — editor-compatible file transfer.
//!
//! The Vue editor downloads `app.sab`, manifests, and kit binaries from
//! the target device and uploads modified content back. The Fantom pod
//! translates these HTTP calls into SOX DASP file ops; Sandstar is the
//! device, so we serve files directly off disk under a configurable
//! root.
//!
//! Path safety: every URI is resolved against `resolve_file_root()` and
//! must stay inside it. `..` segments, absolute paths, and NUL bytes are
//! rejected upstream of `tokio::fs`.
//!
//! Env var `SANDSTAR_FILE_ROOT` selects the on-disk root. If unset, the
//! handlers fall back to `$SANDSTAR_WEB_DIR/files` (so a one-env setup
//! works), and if that's unset too, return 503 — explicitly refusing to
//! serve from the process CWD.
//!
//! MVP scope: direct GET + PUT. Chunked SOX file ops (`fileOpen` /
//! `fileChunk` / `fileClose`) skipped — HTTP bodies don't hit MTU limits
//! the way DASP UDP frames do, so the editor's `axios.put(blob)` just
//! works with a single round trip. `/kit` install deferred to a follow-up.
//!
//! Phase 14.0G-4 (2026-04-23).

use std::path::{Path, PathBuf};

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::Router;
use tokio::fs;
use tracing::{debug, warn};

use crate::sox::sox_ops::SoxState;

pub const FILE_ROOT_ENV: &str = "SANDSTAR_FILE_ROOT";

/// Resolve the filesystem root for `/file/*` operations.
///
/// Order: `SANDSTAR_FILE_ROOT` → `SANDSTAR_WEB_DIR/files` → None (503).
pub fn resolve_file_root() -> Option<PathBuf> {
    if let Ok(raw) = std::env::var(FILE_ROOT_ENV) {
        if !raw.trim().is_empty() {
            let p = PathBuf::from(raw);
            if p.is_dir() {
                return Some(p);
            }
        }
    }
    if let Ok(raw) = std::env::var(super::editor_assets::WEB_DIR_ENV) {
        if !raw.trim().is_empty() {
            let p = PathBuf::from(raw).join("files");
            if p.is_dir() {
                return Some(p);
            }
        }
    }
    None
}

pub fn router(state: SoxState) -> Router {
    Router::new()
        .route("/file/{*path}", get(get_file).put(put_file))
        .with_state(state)
}

// ── Handlers ───────────────────────────────────────

async fn get_file(AxumPath(uri_path): AxumPath<String>, State(_): State<SoxState>) -> Response {
    let root = match resolve_file_root() {
        Some(r) => r,
        None => return service_unavailable(),
    };
    let disk = match safe_join(&root, &uri_path) {
        Ok(p) => p,
        Err(e) => return bad_request(&e),
    };
    match fs::read(&disk).await {
        Ok(bytes) => {
            let ct = guess_content_type(&disk);
            debug!(path = %uri_path, bytes = bytes.len(), "serving file");
            (StatusCode::OK, [(header::CONTENT_TYPE, ct)], bytes).into_response()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            (StatusCode::NOT_FOUND, format!("file not found: {uri_path}")).into_response()
        }
        Err(e) => {
            warn!(path = %uri_path, error = %e, "file read failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "file read failed").into_response()
        }
    }
}

async fn put_file(
    AxumPath(uri_path): AxumPath<String>,
    State(_): State<SoxState>,
    body: Bytes,
) -> Response {
    let root = match resolve_file_root() {
        Some(r) => r,
        None => return service_unavailable(),
    };
    let disk = match safe_join(&root, &uri_path) {
        Ok(p) => p,
        Err(e) => return bad_request(&e),
    };
    if let Some(parent) = disk.parent() {
        if let Err(e) = fs::create_dir_all(parent).await {
            warn!(path = %uri_path, error = %e, "mkdir -p failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "mkdir failed").into_response();
        }
    }
    match fs::write(&disk, &body).await {
        Ok(()) => {
            debug!(path = %uri_path, bytes = body.len(), "wrote file");
            (StatusCode::OK, format!("wrote {} bytes", body.len())).into_response()
        }
        Err(e) => {
            warn!(path = %uri_path, error = %e, "file write failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "file write failed").into_response()
        }
    }
}

// ── Helpers ────────────────────────────────────────

fn service_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "file root not configured (set SANDSTAR_FILE_ROOT or SANDSTAR_WEB_DIR/files)",
    )
        .into_response()
}

fn bad_request(reason: &str) -> Response {
    (StatusCode::BAD_REQUEST, reason.to_string()).into_response()
}

/// Join `root` with a URI-supplied relative path, rejecting any segment
/// that could escape the root (`..`, absolute, drive-prefix, NUL).
pub(crate) fn safe_join(root: &Path, uri: &str) -> Result<PathBuf, String> {
    if uri.contains('\0') {
        return Err("path contains NUL".into());
    }
    let cleaned = uri.trim_start_matches('/');
    if cleaned.is_empty() {
        return Err("empty path".into());
    }
    let mut buf = root.to_path_buf();
    for seg in cleaned.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return Err("'..' is not allowed".into());
        }
        // Reject absolute-like segments (Windows drive letters, leading backslash).
        if seg.contains('\\') || (seg.len() >= 2 && seg.as_bytes()[1] == b':') {
            return Err("invalid path segment".into());
        }
        buf.push(seg);
    }
    // Final sanity check: resolved path must still start with root.
    // We don't canonicalise (the target may not exist for PUT) — instead
    // we forbid parent-escapes upstream, so this is structural already.
    if !buf.starts_with(root) {
        return Err("path escapes root".into());
    }
    Ok(buf)
}

fn guess_content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("sab") => "application/octet-stream",
        Some("sax") => "text/xml; charset=utf-8",
        Some("xml") => "text/xml; charset=utf-8",
        Some("json") => "application/json",
        Some("txt") => "text/plain; charset=utf-8",
        Some("kit") => "application/octet-stream",
        _ => "application/octet-stream",
    }
}

// ── Tests ──────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_accepts_plain_segments() {
        let root = PathBuf::from("/srv/root");
        let r = safe_join(&root, "app.sab").unwrap();
        assert_eq!(r, PathBuf::from("/srv/root/app.sab"));
    }

    #[test]
    fn safe_join_accepts_nested() {
        let root = PathBuf::from("/srv/root");
        let r = safe_join(&root, "kits/control/ConstFloat.kit").unwrap();
        assert_eq!(
            r,
            PathBuf::from("/srv/root/kits/control/ConstFloat.kit")
        );
    }

    #[test]
    fn safe_join_strips_leading_slash() {
        let root = PathBuf::from("/srv/root");
        let r = safe_join(&root, "/app.sab").unwrap();
        assert_eq!(r, PathBuf::from("/srv/root/app.sab"));
    }

    #[test]
    fn safe_join_rejects_dotdot() {
        let root = PathBuf::from("/srv/root");
        let e = safe_join(&root, "../etc/passwd").unwrap_err();
        assert!(e.contains(".."));
    }

    #[test]
    fn safe_join_rejects_nul() {
        let root = PathBuf::from("/srv/root");
        let e = safe_join(&root, "app.sab\0.bak").unwrap_err();
        assert!(e.contains("NUL"));
    }

    #[test]
    fn safe_join_rejects_backslash_segment() {
        let root = PathBuf::from("/srv/root");
        let e = safe_join(&root, "foo\\bar").unwrap_err();
        assert!(e.contains("invalid"));
    }

    #[test]
    fn safe_join_rejects_drive_letter_segment() {
        let root = PathBuf::from("/srv/root");
        let e = safe_join(&root, "C:/etc/passwd").unwrap_err();
        assert!(e.contains("invalid"));
    }

    #[test]
    fn safe_join_rejects_empty() {
        let root = PathBuf::from("/srv/root");
        assert!(safe_join(&root, "").is_err());
        assert!(safe_join(&root, "/").is_err());
    }

    #[test]
    fn content_type_maps_extensions() {
        assert_eq!(guess_content_type(Path::new("a.sab")), "application/octet-stream");
        assert_eq!(guess_content_type(Path::new("a.sax")), "text/xml; charset=utf-8");
        assert_eq!(guess_content_type(Path::new("a.json")), "application/json");
        assert_eq!(guess_content_type(Path::new("a.unknown")), "application/octet-stream");
    }

    #[tokio::test]
    async fn get_file_returns_503_without_root() {
        std::env::remove_var(FILE_ROOT_ENV);
        std::env::remove_var(super::super::editor_assets::WEB_DIR_ENV);
        let tree = std::sync::Arc::new(std::sync::RwLock::new(
            crate::sox::sox_handlers::ComponentTree::new_with_manifest(std::sync::Arc::new(
                crate::sox::sox_handlers::ManifestDb::load(""),
            )),
        ));
        let state = SoxState {
            tree,
            manifest_db: std::sync::Arc::new(crate::sox::sox_handlers::ManifestDb::load("")),
            dyn_store: None,
        };
        let response = get_file(AxumPath("app.sab".into()), State(state)).await;
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "expected 503 when root unset"
        );
    }

    #[tokio::test]
    async fn put_then_get_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var(FILE_ROOT_ENV, tmp.path());
        let state = SoxState {
            tree: std::sync::Arc::new(std::sync::RwLock::new(
                crate::sox::sox_handlers::ComponentTree::new_with_manifest(std::sync::Arc::new(
                    crate::sox::sox_handlers::ManifestDb::load(""),
                )),
            )),
            manifest_db: std::sync::Arc::new(crate::sox::sox_handlers::ManifestDb::load("")),
            dyn_store: None,
        };
        let body = Bytes::from_static(b"hello sab");
        let resp = put_file(
            AxumPath("demo/app.sab".into()),
            State(state.clone()),
            body.clone(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let got = get_file(AxumPath("demo/app.sab".into()), State(state)).await;
        assert_eq!(got.status(), StatusCode::OK);
        // Body check via axum is awkward without full pipeline — the
        // earlier PUT having written to disk, we verify via direct fs.
        let disk = tmp.path().join("demo/app.sab");
        assert_eq!(std::fs::read(&disk).unwrap(), b"hello sab");
        std::env::remove_var(FILE_ROOT_ENV);
    }
}
