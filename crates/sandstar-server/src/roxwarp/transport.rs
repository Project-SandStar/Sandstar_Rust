//! Which transport the roxWarp cluster may use (R-54, S-36).
//!
//! Fail-closed: peers talk over mTLS, or over plain WebSocket only when the
//! operator asked for it (`--cluster-insecure`). A broken or incomplete mTLS
//! setup never falls back to plain WebSocket; the cluster network stays off
//! and the reason is logged.

use std::fmt::Display;
use std::sync::Arc;

use tracing::{error, warn};

use super::cluster::ClusterConfig;
use super::mtls::{build_mtls_client_config, build_mtls_server_config};

/// Transport decision for one side (inbound listener or outbound peers).
#[derive(Debug)]
pub enum Transport<T> {
    /// mTLS with the built rustls config.
    Mtls(T),
    /// Plain WebSocket, no peer authentication (`--cluster-insecure`).
    Plain,
    /// No cluster network on this side.
    Off,
}

impl<T> Transport<T> {
    /// Short name for logs and tests.
    pub fn kind(&self) -> &'static str {
        match self {
            Transport::Mtls(_) => "mtls",
            Transport::Plain => "plain",
            Transport::Off => "off",
        }
    }
}

/// True when the cluster runs over plain WebSocket: `--cluster-insecure`
/// and no mTLS path set. Same rule as the `Plain` arm of [`server_transport`],
/// without building TLS or logging. The REST port mounts `/roxwarp` only then;
/// with mTLS, peers must use the cluster port.
pub fn plain_ws_allowed(config: &ClusterConfig) -> bool {
    config.allow_plain
        && config.cert_path.is_none()
        && config.key_path.is_none()
        && config.ca_path.is_none()
}

/// Transport for the inbound listener (cluster port, default 7443).
pub fn server_transport(config: &ClusterConfig) -> Transport<Arc<rustls::ServerConfig>> {
    decide(config, "listener", build_mtls_server_config)
}

/// Transport for outbound peer connections.
pub fn client_transport(config: &ClusterConfig) -> Transport<Arc<rustls::ClientConfig>> {
    decide(config, "outbound", build_mtls_client_config)
}

fn decide<T, E: Display>(
    config: &ClusterConfig,
    side: &str,
    build: impl FnOnce(&str, &str, &str) -> Result<T, E>,
) -> Transport<T> {
    match (&config.cert_path, &config.key_path, &config.ca_path) {
        (Some(cert), Some(key), Some(ca)) => match build(cert, key, ca) {
            Ok(tls) => Transport::Mtls(tls),
            Err(e) => {
                error!(
                    side,
                    error = %e,
                    "roxWarp: mTLS config failed, cluster {side} disabled (no plain WS fallback)"
                );
                Transport::Off
            }
        },
        (None, None, None) if config.allow_plain => {
            warn!(
                side,
                "roxWarp: cluster {side} uses plain WS without peer auth (--cluster-insecure)"
            );
            Transport::Plain
        }
        (None, None, None) => {
            warn!(
                side,
                "roxWarp: no mTLS configured, cluster {side} disabled; \
                 pass --cluster-cert/--cluster-key/--cluster-ca or --cluster-insecure"
            );
            Transport::Off
        }
        _ => {
            error!(
                side,
                "roxWarp: incomplete mTLS config (cert, key and ca are all required), \
                 cluster {side} disabled"
            );
            Transport::Off
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(certs: Option<(&str, &str, &str)>, allow_plain: bool) -> ClusterConfig {
        let mut c = ClusterConfig {
            allow_plain,
            ..ClusterConfig::default()
        };
        if let Some((cert, key, ca)) = certs {
            c.cert_path = Some(cert.to_string());
            c.key_path = Some(key.to_string());
            c.ca_path = Some(ca.to_string());
        }
        c
    }

    /// Files that exist but are not PEM certificates or keys.
    fn bogus_files() -> (tempfile::TempDir, String, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for name in ["cert.pem", "key.pem", "ca.pem"] {
            let p = dir.path().join(name);
            std::fs::write(&p, "not a pem file\n").unwrap();
            paths.push(p.to_string_lossy().into_owned());
        }
        let ca = paths.pop().unwrap();
        let key = paths.pop().unwrap();
        let cert = paths.pop().unwrap();
        (dir, cert, key, ca)
    }

    #[test]
    fn no_certs_and_no_flag_is_off() {
        let c = config(None, false);
        assert_eq!(server_transport(&c).kind(), "off");
        assert_eq!(client_transport(&c).kind(), "off");
    }

    #[test]
    fn no_certs_with_insecure_flag_is_plain() {
        let c = config(None, true);
        assert_eq!(server_transport(&c).kind(), "plain");
        assert_eq!(client_transport(&c).kind(), "plain");
    }

    #[test]
    fn broken_certs_are_off_not_plain() {
        let (_dir, cert, key, ca) = bogus_files();
        for allow_plain in [false, true] {
            let c = config(Some((&cert, &key, &ca)), allow_plain);
            assert_eq!(
                server_transport(&c).kind(),
                "off",
                "allow_plain={allow_plain}"
            );
            assert_eq!(
                client_transport(&c).kind(),
                "off",
                "allow_plain={allow_plain}"
            );
        }
    }

    #[test]
    fn missing_cert_files_are_off() {
        let c = config(
            Some((
                "/nonexistent/c.pem",
                "/nonexistent/k.pem",
                "/nonexistent/ca.pem",
            )),
            true,
        );
        assert_eq!(server_transport(&c).kind(), "off");
        assert_eq!(client_transport(&c).kind(), "off");
    }

    #[test]
    fn incomplete_mtls_config_is_off_even_with_insecure_flag() {
        let mut c = config(None, true);
        c.cert_path = Some("/some/cert.pem".to_string());
        assert_eq!(server_transport(&c).kind(), "off");
        assert_eq!(client_transport(&c).kind(), "off");
    }

    #[test]
    fn plain_ws_allowed_matches_the_plain_transport() {
        let (_dir, cert, key, ca) = bogus_files();
        let mut partial = config(None, true);
        partial.ca_path = Some(ca.clone());
        for c in [
            config(None, false),
            config(None, true),
            config(Some((&cert, &key, &ca)), false),
            config(Some((&cert, &key, &ca)), true),
            partial,
        ] {
            assert_eq!(
                plain_ws_allowed(&c),
                server_transport(&c).kind() == "plain",
                "allow_plain={} certs={:?}",
                c.allow_plain,
                c.cert_path
            );
        }
    }

    #[test]
    fn built_mtls_config_is_used() {
        let c = config(Some(("c", "k", "ca")), false);
        let t = decide(&c, "listener", |_, _, _| Ok::<_, String>(42u8));
        assert!(matches!(t, Transport::Mtls(42)));
    }
}
