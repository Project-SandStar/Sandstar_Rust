//! Editor asset serving — static HTML fallback + Vue SPA when installed.
//!
//! The Sandstar binary ships without a Vue bundle. When the operator (or
//! `cargo-deb` postinst) drops the compiled Vue editor into a directory on
//! disk and points `SANDSTAR_WEB_DIR` at it, the `/editor/*` routes serve
//! the SPA. Otherwise the router keeps the existing `/editor` placeholder
//! so the binary is self-sufficient for dashboard/health-check use.
//!
//! Build workflow for the operator:
//! ```text
//! cd C:\Coding\swe\sedonaWebEditor
//! npm run build                       # produces ./dist/
//! export SANDSTAR_WEB_DIR=$(pwd)/dist # or install to /home/eacio/sandstar/web/
//! cargo run -p sandstar-server        # now /editor serves the SPA
//! ```
//!
//! Production `.deb` layout (target, not wired yet): `/home/eacio/sandstar/web/`
//! with the service's `EnvironmentFile=` setting `SANDSTAR_WEB_DIR=/home/eacio/sandstar/web`.
//!
//! Phase 14.0G-7 skeleton (2026-04-23).

use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tower_http::services::ServeDir;
use tracing::{info, warn};

const EDITOR_HTML: &str = include_str!("editor.html");

/// Env var the operator sets to the directory containing the compiled Vue
/// bundle (typically `sedonaWebEditor/dist` or `/home/eacio/sandstar/web`).
pub const WEB_DIR_ENV: &str = "SANDSTAR_WEB_DIR";

/// Resolve the configured web directory, returning `None` if unset, empty,
/// or non-existent. We validate existence at startup so the caller can fall
/// back cleanly — an invalid path is logged and treated as "no SPA."
pub fn resolve_web_dir() -> Option<PathBuf> {
    let raw = std::env::var(WEB_DIR_ENV).ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    if !path.exists() {
        warn!(
            path = %path.display(),
            "SANDSTAR_WEB_DIR is set but the directory does not exist — falling back to placeholder /editor"
        );
        return None;
    }
    if !path.is_dir() {
        warn!(
            path = %path.display(),
            "SANDSTAR_WEB_DIR points at a non-directory — falling back to placeholder /editor"
        );
        return None;
    }
    Some(path)
}

/// Build the `/editor` router. If the SPA is installed, serves the bundle
/// under `/editor/*` with `index.html` as the SPA fallback. Otherwise
/// serves the single-page placeholder at `/editor` only.
pub fn router() -> Router {
    match resolve_web_dir() {
        Some(path) => {
            info!(path = %path.display(), "Serving Vue editor bundle at /editor/*");
            spa_router(&path)
        }
        None => placeholder_router(),
    }
}

fn placeholder_router() -> Router {
    Router::new().route("/editor", get(placeholder_handler))
}

async fn placeholder_handler() -> Response {
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        EDITOR_HTML,
    )
        .into_response()
}

fn spa_router(dir: &Path) -> Router {
    // Serve files from the asset dir under /editor/. `not_found_service`
    // set to the dir's `index.html` gives standard SPA-style routing —
    // any missing file returns the index so client-side routes work.
    let index = dir.join("index.html");
    let serve = ServeDir::new(dir)
        .append_index_html_on_directories(true)
        .not_found_service(ServeDir::new(dir).append_index_html_on_directories(true));
    // We register two paths — `/editor` and `/editor/*` — so the bare
    // `/editor` URL resolves to `index.html` the same as `/editor/`.
    let index_route = {
        let idx = index.clone();
        get(move || {
            let idx = idx.clone();
            async move {
                match tokio::fs::read_to_string(&idx).await {
                    Ok(html) => (
                        StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
                        html,
                    )
                        .into_response(),
                    Err(_) => (StatusCode::NOT_FOUND, "editor index.html missing").into_response(),
                }
            }
        })
    };
    Router::new()
        .route("/editor", index_route)
        .nest_service("/editor/", serve)
}

// ── Tests ──────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises env-var-mutating tests across this module AND
    /// `file_ops::tests`, which also inspects `WEB_DIR_ENV` as a
    /// fallback. A single process-wide lock keeps parallel test
    /// threads from corrupting each other's env-var assumptions.
    fn env_lock() -> &'static std::sync::Mutex<()> {
        super::super::file_ops::test_env_lock()
    }

    #[test]
    fn unset_env_returns_none() {
        let _g = env_lock().lock().unwrap();
        std::env::remove_var(WEB_DIR_ENV);
        assert!(resolve_web_dir().is_none());
    }

    #[test]
    fn nonexistent_path_returns_none() {
        let _g = env_lock().lock().unwrap();
        std::env::set_var(WEB_DIR_ENV, "/definitely/not/a/real/path/xyz123");
        assert!(resolve_web_dir().is_none());
        std::env::remove_var(WEB_DIR_ENV);
    }

    #[test]
    fn empty_env_returns_none() {
        let _g = env_lock().lock().unwrap();
        std::env::set_var(WEB_DIR_ENV, "   ");
        assert!(resolve_web_dir().is_none());
        std::env::remove_var(WEB_DIR_ENV);
    }
}
