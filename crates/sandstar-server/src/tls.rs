//! TLS configuration for HTTPS server support.
//!
//! Loads PEM-encoded certificate and private key files for use with
//! `axum-server`'s rustls integration. Only compiled when the `tls`
//! feature is enabled.
//!
//! # Usage
//!
//! ```bash
//! # Generate self-signed cert for development/embedded devices:
//! openssl req -x509 -newkey rsa:2048 -keyout key.pem -out cert.pem \
//!     -days 365 -nodes -subj '/CN=sandstar'
//!
//! # Start server with TLS:
//! cargo run -p sandstar-server --features tls -- --tls-cert cert.pem --tls-key key.pem
//! ```

use std::path::Path;

/// Validate that TLS cert and key arguments are consistent.
///
/// Returns an error message if only one of cert/key is provided.
/// Returns `Ok(true)` if TLS should be enabled, `Ok(false)` if not.
pub fn validate_tls_args(
    tls_cert: &Option<std::path::PathBuf>,
    tls_key: &Option<std::path::PathBuf>,
) -> Result<bool, String> {
    match (tls_cert, tls_key) {
        (Some(_), Some(_)) => Ok(true),
        (None, None) => Ok(false),
        (Some(_), None) => Err("--tls-cert requires --tls-key".to_string()),
        (None, Some(_)) => Err("--tls-key requires --tls-cert".to_string()),
    }
}

/// Validate that the cert and key files exist on disk.
///
/// Called after `validate_tls_args` returns `Ok(true)`.
pub fn validate_tls_files(cert_path: &Path, key_path: &Path) -> Result<(), String> {
    if !cert_path.exists() {
        return Err(format!(
            "TLS certificate file not found: {}",
            cert_path.display()
        ));
    }
    if !key_path.exists() {
        return Err(format!(
            "TLS key file not found: {}",
            key_path.display()
        ));
    }
    Ok(())
}

/// Build an `axum_server::tls_rustls::RustlsConfig` from PEM files.
///
/// Only available when the `tls` feature is enabled.
#[cfg(feature = "tls")]
pub async fn load_rustls_config(
    cert_path: &Path,
    key_path: &Path,
) -> Result<axum_server::tls_rustls::RustlsConfig, String> {
    axum_server::tls_rustls::RustlsConfig::from_pem_file(cert_path, key_path)
        .await
        .map_err(|e| format!("failed to load TLS config: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn validate_both_present() {
        let cert = Some(PathBuf::from("cert.pem"));
        let key = Some(PathBuf::from("key.pem"));
        assert_eq!(validate_tls_args(&cert, &key), Ok(true));
    }

    #[test]
    fn validate_both_absent() {
        let cert: Option<PathBuf> = None;
        let key: Option<PathBuf> = None;
        assert_eq!(validate_tls_args(&cert, &key), Ok(false));
    }

    #[test]
    fn validate_cert_without_key() {
        let cert = Some(PathBuf::from("cert.pem"));
        let key: Option<PathBuf> = None;
        let result = validate_tls_args(&cert, &key);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("--tls-cert requires --tls-key"));
    }

    #[test]
    fn validate_key_without_cert() {
        let cert: Option<PathBuf> = None;
        let key = Some(PathBuf::from("key.pem"));
        let result = validate_tls_args(&cert, &key);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("--tls-key requires --tls-cert"));
    }

    #[test]
    fn validate_files_missing_cert() {
        let result = validate_tls_files(
            Path::new("/nonexistent/cert.pem"),
            Path::new("/nonexistent/key.pem"),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("certificate file not found"));
    }

    #[test]
    fn validate_files_missing_key() {
        // Create a temp file for cert, but use a nonexistent key
        let dir = tempfile::TempDir::new().unwrap();
        let cert_path = dir.path().join("cert.pem");
        std::fs::write(&cert_path, "fake cert").unwrap();

        let result = validate_tls_files(&cert_path, Path::new("/nonexistent/key.pem"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("key file not found"));
    }

    #[test]
    fn validate_files_both_exist() {
        let dir = tempfile::TempDir::new().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, "fake cert").unwrap();
        std::fs::write(&key_path, "fake key").unwrap();

        assert!(validate_tls_files(&cert_path, &key_path).is_ok());
    }
}
