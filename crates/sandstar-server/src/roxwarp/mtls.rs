//! mTLS configuration for roxWarp device-to-device authentication.
//!
//! Builds `rustls` server and client configs that require mutual TLS:
//! - **Server config**: Used by the roxWarp listener on port 7443, requires
//!   client certificates signed by the cluster CA.
//! - **Client config**: Used by outbound peer connections, presents the
//!   device certificate to the remote peer for verification.

use std::fs;
use std::io::BufReader;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use super::RoxWarpError;

/// Build a rustls `ServerConfig` with mTLS (client certificate required).
///
/// The server presents `cert_path`/`key_path` as its identity and requires
/// connecting clients to present a certificate signed by the CA at `ca_path`.
pub fn build_mtls_server_config(
    cert_path: &str,
    key_path: &str,
    ca_path: &str,
) -> Result<Arc<rustls::ServerConfig>, RoxWarpError> {
    let certs = load_certs(cert_path)?;
    let key = load_private_key(key_path)?;
    let ca_certs = load_certs(ca_path)?;

    // Build a root cert store from the CA certificate(s)
    let mut root_store = rustls::RootCertStore::empty();
    for ca in &ca_certs {
        root_store
            .add(ca.clone())
            .map_err(|e| RoxWarpError::Connection(format!("failed to add CA cert: {e}")))?;
    }

    // Require client authentication with certs signed by the CA
    let client_verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store))
        .build()
        .map_err(|e| RoxWarpError::Connection(format!("failed to build client verifier: {e}")))?;

    let config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(certs, key)
        .map_err(|e| RoxWarpError::Connection(format!("failed to build server TLS config: {e}")))?;

    Ok(Arc::new(config))
}

/// Build a rustls `ClientConfig` with mTLS (device cert for outbound connections).
///
/// The client presents `cert_path`/`key_path` as its identity and verifies
/// the server certificate against the CA at `ca_path`.
pub fn build_mtls_client_config(
    cert_path: &str,
    key_path: &str,
    ca_path: &str,
) -> Result<Arc<rustls::ClientConfig>, RoxWarpError> {
    let certs = load_certs(cert_path)?;
    let key = load_private_key(key_path)?;
    let ca_certs = load_certs(ca_path)?;

    // Build a root cert store from the CA certificate(s)
    let mut root_store = rustls::RootCertStore::empty();
    for ca in &ca_certs {
        root_store
            .add(ca.clone())
            .map_err(|e| RoxWarpError::Connection(format!("failed to add CA cert: {e}")))?;
    }

    let config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_client_auth_cert(certs, key)
        .map_err(|e| RoxWarpError::Connection(format!("failed to build client TLS config: {e}")))?;

    Ok(Arc::new(config))
}

/// Validate that all three mTLS paths are provided together.
///
/// Returns `Ok(true)` if mTLS is configured, `Ok(false)` if none are set,
/// or `Err` if only some are provided.
pub fn validate_mtls_args(
    cert: &Option<String>,
    key: &Option<String>,
    ca: &Option<String>,
) -> Result<bool, String> {
    match (cert.is_some(), key.is_some(), ca.is_some()) {
        (true, true, true) => Ok(true),
        (false, false, false) => Ok(false),
        _ => Err(
            "--cluster-cert, --cluster-key, and --cluster-ca must all be provided together"
                .to_string(),
        ),
    }
}

// ── PEM loading helpers ──────────────────────────────

/// Load PEM-encoded certificates from a file.
fn load_certs(path: &str) -> Result<Vec<CertificateDer<'static>>, RoxWarpError> {
    let file = fs::File::open(path)
        .map_err(|e| RoxWarpError::Connection(format!("open cert {path}: {e}")))?;
    let mut reader = BufReader::new(file);
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| RoxWarpError::Connection(format!("parse certs {path}: {e}")))?;
    if certs.is_empty() {
        return Err(RoxWarpError::Connection(format!(
            "no certificates found in {path}"
        )));
    }
    Ok(certs)
}

/// Load a PEM-encoded private key from a file.
fn load_private_key(path: &str) -> Result<PrivateKeyDer<'static>, RoxWarpError> {
    let file = fs::File::open(path)
        .map_err(|e| RoxWarpError::Connection(format!("open key {path}: {e}")))?;
    let mut reader = BufReader::new(file);

    // Try PKCS8 first, then RSA, then EC
    for item in rustls_pemfile::read_all(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| RoxWarpError::Connection(format!("parse key {path}: {e}")))?
    {
        match item {
            rustls_pemfile::Item::Pkcs8Key(key) => {
                return Ok(PrivateKeyDer::Pkcs8(key));
            }
            rustls_pemfile::Item::Pkcs1Key(key) => {
                return Ok(PrivateKeyDer::Pkcs1(key));
            }
            rustls_pemfile::Item::Sec1Key(key) => {
                return Ok(PrivateKeyDer::Sec1(key));
            }
            _ => continue,
        }
    }

    Err(RoxWarpError::Connection(format!(
        "no private key found in {path}"
    )))
}

// ── Tests ────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_all_present() {
        let cert = Some("/path/cert.pem".to_string());
        let key = Some("/path/key.pem".to_string());
        let ca = Some("/path/ca.pem".to_string());
        assert_eq!(validate_mtls_args(&cert, &key, &ca), Ok(true));
    }

    #[test]
    fn validate_none_present() {
        assert_eq!(validate_mtls_args(&None, &None, &None), Ok(false));
    }

    #[test]
    fn validate_partial_is_error() {
        let cert = Some("/path/cert.pem".to_string());
        assert!(validate_mtls_args(&cert, &None, &None).is_err());
        assert!(validate_mtls_args(&None, &cert, &None).is_err());
        assert!(validate_mtls_args(&None, &None, &cert).is_err());
        assert!(validate_mtls_args(&cert, &cert, &None).is_err());
    }

    #[test]
    fn load_certs_missing_file() {
        let result = load_certs("/nonexistent/cert.pem");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("open cert"), "got: {err}");
    }

    #[test]
    fn load_key_missing_file() {
        let result = load_private_key("/nonexistent/key.pem");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("open key"), "got: {err}");
    }

    #[test]
    fn load_certs_empty_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("empty.pem");
        std::fs::write(&path, "").unwrap();
        let result = load_certs(path.to_str().unwrap());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("no certificates"), "got: {err}");
    }

    #[test]
    fn load_key_empty_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("empty.pem");
        std::fs::write(&path, "").unwrap();
        let result = load_private_key(path.to_str().unwrap());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("no private key"), "got: {err}");
    }
}
