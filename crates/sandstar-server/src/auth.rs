//! SCRAM-SHA-256 authentication (RFC 5802) for Haystack protocol.
//!
//! Supports both HTTP header-based and WebSocket message-based flows.
//! Backward compatible: bearer tokens still work for simple scripts.
//!
//! ## SCRAM Flow (HTTP)
//!
//! 1. Client sends `Authorization: SCRAM handshakeToken=, data=<base64(client-first)>`
//! 2. Server replies 401 with `WWW-Authenticate: SCRAM handshakeToken=<tok>, data=<base64(server-first)>`
//! 3. Client sends `Authorization: SCRAM handshakeToken=<tok>, data=<base64(client-final)>`
//! 4. Server replies 200 with `Authentication-Info: authToken=<session>, data=<base64(server-final)>`
//!
//! ## SCRAM Flow (WebSocket)
//!
//! 1. Client sends `{"op":"hello","username":"user"}`
//! 2. Server sends `{"op":"challenge","handshakeToken":"tok","hash":"SHA-256","salt":"...","iterations":N}`
//! 3. Client sends `{"op":"authenticate","handshakeToken":"tok","proof":"<base64>"}`
//! 4. Server sends `{"op":"authOk","authToken":"<session>"}`

use std::collections::HashMap;
use std::time::Instant;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as B64Engine;
use hmac::{Hmac, Mac};
use rand::Rng;
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

const DEFAULT_ITERATIONS: u32 = 10_000;
const NONCE_LEN: usize = 24;
const HANDSHAKE_TIMEOUT_SECS: u64 = 30;
const SESSION_LIFETIME_SECS: u64 = 86_400; // 24 hours
const MAX_SESSIONS: usize = 256;

// ── Stored Credential ──────────────────────────────────────

/// Stored credential for a user (server-side, derived from password).
#[derive(Debug, Clone)]
pub struct StoredCredential {
    pub username: String,
    pub salt: Vec<u8>,
    pub iterations: u32,
    pub stored_key: [u8; 32],
    pub server_key: [u8; 32],
}

impl StoredCredential {
    /// Create from a plaintext password (for initial setup / config loading).
    pub fn from_password(username: &str, password: &str) -> Self {
        let mut salt = [0u8; 16];
        rand::rng().fill(&mut salt);
        Self::from_password_with_salt(username, password, &salt, DEFAULT_ITERATIONS)
    }

    /// Create with explicit salt and iteration count (for deterministic tests).
    pub fn from_password_with_salt(
        username: &str,
        password: &str,
        salt: &[u8],
        iterations: u32,
    ) -> Self {
        let salted_password = hi(password.as_bytes(), salt, iterations);
        let client_key = hmac_sha256(&salted_password, b"Client Key");
        let stored_key_hash = sha256(&client_key);
        let server_key = hmac_sha256(&salted_password, b"Server Key");

        let mut stored_key = [0u8; 32];
        stored_key.copy_from_slice(&stored_key_hash);
        let mut sk = [0u8; 32];
        sk.copy_from_slice(&server_key);

        Self {
            username: username.to_string(),
            salt: salt.to_vec(),
            iterations,
            stored_key,
            server_key: sk,
        }
    }
}

// ── Auth Store ─────────────────────────────────────────────

/// Auth store holding all credentials and the legacy bearer token.
#[derive(Debug, Clone, Default)]
pub struct AuthStore {
    credentials: HashMap<String, StoredCredential>,
    /// Legacy bearer token (backward compat with --auth-token).
    bearer_token: Option<String>,
}

impl AuthStore {
    /// Create an empty auth store (no auth required).
    pub fn new() -> Self {
        Self::default()
    }

    /// Create with a legacy bearer token only.
    pub fn with_bearer_token(token: String) -> Self {
        Self {
            credentials: HashMap::new(),
            bearer_token: Some(token),
        }
    }

    /// Add a SCRAM user credential.
    pub fn add_user(&mut self, username: &str, password: &str) {
        let cred = StoredCredential::from_password(username, password);
        self.credentials.insert(username.to_string(), cred);
    }

    /// Set the legacy bearer token.
    pub fn set_bearer_token(&mut self, token: String) {
        self.bearer_token = Some(token);
    }

    /// Get credential for a username.
    pub fn get_credential(&self, username: &str) -> Option<&StoredCredential> {
        self.credentials.get(username)
    }

    /// Check if a bearer token matches.
    pub fn check_bearer(&self, token: &str) -> bool {
        self.bearer_token.as_deref() == Some(token)
    }

    /// Returns true if any auth mechanism is configured (bearer or SCRAM).
    pub fn is_auth_required(&self) -> bool {
        self.bearer_token.is_some() || !self.credentials.is_empty()
    }

    /// Check if SCRAM credentials exist.
    pub fn has_scram_users(&self) -> bool {
        !self.credentials.is_empty()
    }
}

// ── SCRAM Handshake ────────────────────────────────────────

/// Active SCRAM handshake (server-side state machine).
#[derive(Debug)]
pub struct ScramHandshake {
    pub username: String,
    pub client_nonce: String,
    pub server_nonce: String,
    pub client_first_bare: String,
    pub stored: StoredCredential,
    pub created_at: Instant,
}

impl ScramHandshake {
    /// Check if this handshake has expired.
    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed().as_secs() >= HANDSHAKE_TIMEOUT_SECS
    }
}

/// Parsed client-first-message fields.
#[derive(Debug)]
pub struct ClientFirst {
    pub username: String,
    pub nonce: String,
    pub bare: String,
}

/// Parse a SCRAM client-first-message.
///
/// Format: `n,,n=<user>,r=<nonce>`
/// The `bare` part is `n=<user>,r=<nonce>` (after `n,,`).
pub fn parse_client_first(msg: &str) -> Result<ClientFirst, String> {
    // Strip GS2 header: "n,," (no channel binding, no authzid)
    let bare = msg
        .strip_prefix("n,,")
        .ok_or_else(|| "missing GS2 header 'n,,'".to_string())?;

    let mut username = None;
    let mut nonce = None;

    for part in bare.split(',') {
        if let Some(u) = part.strip_prefix("n=") {
            username = Some(u.to_string());
        } else if let Some(r) = part.strip_prefix("r=") {
            nonce = Some(r.to_string());
        }
    }

    let username = username.ok_or_else(|| "missing n= in client-first".to_string())?;
    let nonce = nonce.ok_or_else(|| "missing r= in client-first".to_string())?;

    Ok(ClientFirst {
        username,
        nonce,
        bare: bare.to_string(),
    })
}

/// Generate a random nonce string (base64-encoded random bytes).
pub fn generate_nonce() -> String {
    let mut bytes = [0u8; NONCE_LEN];
    rand::rng().fill(&mut bytes);
    BASE64.encode(bytes)
}

/// Generate a random handshake token (hex-encoded).
pub fn generate_handshake_token() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill(&mut bytes);
    hex_encode(&bytes)
}

/// Build the server-first-message from a handshake.
pub fn scram_server_first(handshake: &ScramHandshake) -> String {
    let combined_nonce = format!("{}{}", handshake.client_nonce, handshake.server_nonce);
    format!(
        "r={},s={},i={}",
        combined_nonce,
        BASE64.encode(&handshake.stored.salt),
        handshake.stored.iterations,
    )
}

/// Parsed client-final-message fields.
#[derive(Debug)]
pub struct ClientFinal {
    pub nonce: String,
    pub proof: Vec<u8>,
    pub without_proof: String,
}

/// Parse a SCRAM client-final-message.
///
/// Format: `c=biws,r=<combined_nonce>,p=<base64_proof>`
pub fn parse_client_final(msg: &str) -> Result<ClientFinal, String> {
    let mut channel_binding = None;
    let mut nonce = None;
    let mut proof_b64 = None;

    for part in msg.split(',') {
        if let Some(c) = part.strip_prefix("c=") {
            channel_binding = Some(c.to_string());
        } else if let Some(r) = part.strip_prefix("r=") {
            nonce = Some(r.to_string());
        } else if let Some(p) = part.strip_prefix("p=") {
            proof_b64 = Some(p.to_string());
        }
    }

    let _cb = channel_binding.ok_or_else(|| "missing c= in client-final".to_string())?;
    let nonce = nonce.ok_or_else(|| "missing r= in client-final".to_string())?;
    let proof_b64 = proof_b64.ok_or_else(|| "missing p= in client-final".to_string())?;

    let proof = BASE64
        .decode(&proof_b64)
        .map_err(|e| format!("invalid proof base64: {e}"))?;

    // without_proof is everything before ",p="
    let without_proof = msg
        .rfind(",p=")
        .map(|i| &msg[..i])
        .ok_or_else(|| "missing ,p= in client-final".to_string())?
        .to_string();

    Ok(ClientFinal {
        nonce,
        proof,
        without_proof,
    })
}

/// Verify the client proof and return the server signature (base64-encoded).
///
/// This is the core of SCRAM verification:
/// 1. Reconstruct the AuthMessage from client-first-bare, server-first, client-final-without-proof
/// 2. Compute ClientSignature = HMAC(StoredKey, AuthMessage)
/// 3. Recover ClientKey = ClientProof XOR ClientSignature
/// 4. Verify H(ClientKey) == StoredKey
/// 5. Compute ServerSignature = HMAC(ServerKey, AuthMessage) for the client to verify us
pub fn scram_verify_client_final(
    handshake: &ScramHandshake,
    client_final_msg: &str,
) -> Result<String, String> {
    if handshake.is_expired() {
        return Err("handshake expired".to_string());
    }

    let client_final = parse_client_final(client_final_msg)?;

    // Verify the nonce matches
    let expected_nonce = format!("{}{}", handshake.client_nonce, handshake.server_nonce);
    if client_final.nonce != expected_nonce {
        return Err("nonce mismatch".to_string());
    }

    // Build the auth message
    let server_first = scram_server_first(handshake);
    let auth_message = format!(
        "{},{},{}",
        handshake.client_first_bare, server_first, client_final.without_proof,
    );

    // ClientSignature = HMAC(StoredKey, AuthMessage)
    let client_signature = hmac_sha256(&handshake.stored.stored_key, auth_message.as_bytes());

    // Recover ClientKey = ClientProof XOR ClientSignature
    if client_final.proof.len() != 32 {
        return Err(format!(
            "invalid proof length: {} (expected 32)",
            client_final.proof.len()
        ));
    }
    let mut recovered_client_key = [0u8; 32];
    for i in 0..32 {
        recovered_client_key[i] = client_final.proof[i] ^ client_signature[i];
    }

    // Verify: H(recovered_client_key) == StoredKey
    let hashed = sha256(&recovered_client_key);
    if hashed != handshake.stored.stored_key {
        return Err("invalid client proof (wrong password)".to_string());
    }

    // ServerSignature = HMAC(ServerKey, AuthMessage)
    let server_signature = hmac_sha256(&handshake.stored.server_key, auth_message.as_bytes());
    Ok(BASE64.encode(server_signature))
}

// ── Session Management ─────────────────────────────────────

/// Session token issued after successful SCRAM auth.
#[derive(Debug, Clone)]
pub struct AuthSession {
    pub token: String,
    pub username: String,
    pub expires_at: Instant,
}

impl AuthSession {
    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}

/// Session store for managing active auth sessions.
#[derive(Debug)]
pub struct SessionStore {
    sessions: HashMap<String, AuthSession>,
    max_sessions: usize,
}

impl SessionStore {
    pub fn new(max: usize) -> Self {
        Self {
            sessions: HashMap::new(),
            max_sessions: max,
        }
    }

    /// Create a new session for the given username.
    pub fn create_session(&mut self, username: &str) -> AuthSession {
        // If at capacity, evict expired sessions first
        if self.sessions.len() >= self.max_sessions {
            self.expire_stale();
        }
        // If still at capacity, remove the oldest session
        if self.sessions.len() >= self.max_sessions {
            if let Some(oldest_token) = self
                .sessions
                .iter()
                .min_by_key(|(_, s)| s.expires_at)
                .map(|(k, _)| k.clone())
            {
                self.sessions.remove(&oldest_token);
            }
        }

        let mut token_bytes = [0u8; 32];
        rand::rng().fill(&mut token_bytes);
        let token = hex_encode(&token_bytes);

        let session = AuthSession {
            token: token.clone(),
            username: username.to_string(),
            expires_at: Instant::now() + std::time::Duration::from_secs(SESSION_LIFETIME_SECS),
        };

        self.sessions.insert(token, session.clone());
        session
    }

    /// Validate a session token and return the session if valid.
    pub fn validate_token(&self, token: &str) -> Option<&AuthSession> {
        self.sessions.get(token).filter(|s| !s.is_expired())
    }

    /// Remove expired sessions.
    pub fn expire_stale(&mut self) {
        self.sessions.retain(|_, s| !s.is_expired());
    }

    /// Number of active sessions.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Returns true if there are no active sessions.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

// ── Shared Auth State ──────────────────────────────────────

/// Shared authentication state, passed to Axum handlers.
///
/// `AuthStore` is immutable after creation. Sessions and handshakes
/// are behind `RwLock` for concurrent handler access.
#[derive(Clone)]
pub struct AuthState {
    pub store: AuthStore,
    pub sessions: std::sync::Arc<std::sync::RwLock<SessionStore>>,
    pub handshakes: std::sync::Arc<std::sync::RwLock<HashMap<String, ScramHandshake>>>,
}

impl AuthState {
    /// Create a new AuthState from an AuthStore.
    pub fn new(store: AuthStore) -> Self {
        Self {
            store,
            sessions: std::sync::Arc::new(std::sync::RwLock::new(SessionStore::new(MAX_SESSIONS))),
            handshakes: std::sync::Arc::new(std::sync::RwLock::new(HashMap::new())),
        }
    }

    /// Clean up expired handshakes and sessions.
    pub fn cleanup_expired(&self) {
        if let Ok(mut hs) = self.handshakes.write() {
            hs.retain(|_, h| !h.is_expired());
        }
        if let Ok(mut ss) = self.sessions.write() {
            ss.expire_stale();
        }
    }

    /// Begin a SCRAM handshake for the given client-first-message.
    /// Returns (handshake_token, server-first-message) or an error.
    pub fn begin_scram(&self, client_first_msg: &str) -> Result<(String, String), String> {
        let cf = parse_client_first(client_first_msg)?;

        let stored = self
            .store
            .get_credential(&cf.username)
            .ok_or_else(|| "unknown user".to_string())?
            .clone();

        let server_nonce = generate_nonce();
        let handshake_token = generate_handshake_token();

        let handshake = ScramHandshake {
            username: cf.username,
            client_nonce: cf.nonce,
            server_nonce,
            client_first_bare: cf.bare,
            stored,
            created_at: Instant::now(),
        };

        let server_first = scram_server_first(&handshake);

        if let Ok(mut hs) = self.handshakes.write() {
            // Clean up expired handshakes while we're at it
            hs.retain(|_, h| !h.is_expired());
            hs.insert(handshake_token.clone(), handshake);
        }

        Ok((handshake_token, server_first))
    }

    /// Complete a SCRAM handshake. Returns (session_token, server_signature) on success.
    pub fn complete_scram(
        &self,
        handshake_token: &str,
        client_final_msg: &str,
    ) -> Result<(String, String), String> {
        let handshake = {
            let mut hs = self
                .handshakes
                .write()
                .map_err(|_| "lock poisoned".to_string())?;
            hs.remove(handshake_token)
                .ok_or_else(|| "invalid or expired handshake token".to_string())?
        };

        let server_sig = scram_verify_client_final(&handshake, client_final_msg)?;

        let session = {
            let mut ss = self
                .sessions
                .write()
                .map_err(|_| "lock poisoned".to_string())?;
            ss.create_session(&handshake.username)
        };

        Ok((session.token, server_sig))
    }

    /// Check if a bearer or session token is valid.
    /// Returns true if:
    /// - No auth is configured, or
    /// - The token matches the legacy bearer token, or
    /// - The token matches a valid session token.
    pub fn check_token(&self, token: &str) -> bool {
        if !self.store.is_auth_required() {
            return true;
        }
        if self.store.check_bearer(token) {
            return true;
        }
        if let Ok(ss) = self.sessions.read() {
            if ss.validate_token(token).is_some() {
                return true;
            }
        }
        false
    }
}

// ── Crypto Primitives ──────────────────────────────────────

/// Hi(password, salt, iterations) = PBKDF2-SHA-256
fn hi(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut output = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(password, salt, iterations, &mut output);
    output
}

/// HMAC-SHA-256(key, msg)
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(msg);
    let result = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

/// SHA-256(data)
fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

/// Hex-encode bytes.
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Simulate a full SCRAM client-side handshake for testing.
/// Returns (client_first_message, client_nonce).
pub fn scram_client_first(username: &str) -> (String, String) {
    let nonce = generate_nonce();
    let bare = format!("n={},r={}", username, nonce);
    let full = format!("n,,{}", bare);
    (full, nonce)
}

/// Build client-final-message given server-first and password.
pub fn scram_client_final(
    password: &str,
    client_nonce: &str,
    client_first_bare: &str,
    server_first: &str,
) -> Result<String, String> {
    // Parse server-first: r=<combined_nonce>,s=<salt_b64>,i=<iterations>
    let mut combined_nonce = None;
    let mut salt_b64 = None;
    let mut iterations = None;

    for part in server_first.split(',') {
        if let Some(r) = part.strip_prefix("r=") {
            combined_nonce = Some(r.to_string());
        } else if let Some(s) = part.strip_prefix("s=") {
            salt_b64 = Some(s.to_string());
        } else if let Some(i) = part.strip_prefix("i=") {
            iterations = Some(
                i.parse::<u32>()
                    .map_err(|e| format!("bad iteration count: {e}"))?,
            );
        }
    }

    let combined_nonce = combined_nonce.ok_or_else(|| "missing r= in server-first".to_string())?;
    let salt_b64 = salt_b64.ok_or_else(|| "missing s= in server-first".to_string())?;
    let iterations = iterations.ok_or_else(|| "missing i= in server-first".to_string())?;

    // Verify server nonce starts with client nonce
    if !combined_nonce.starts_with(client_nonce) {
        return Err("server nonce doesn't start with client nonce".to_string());
    }

    let salt = BASE64
        .decode(&salt_b64)
        .map_err(|e| format!("bad salt base64: {e}"))?;

    // Derive keys
    let salted_password = hi(password.as_bytes(), &salt, iterations);
    let client_key = hmac_sha256(&salted_password, b"Client Key");
    let stored_key = sha256(&client_key);

    // Build auth message
    let client_final_without_proof = format!("c=biws,r={}", combined_nonce);
    let auth_message = format!(
        "{},{},{}",
        client_first_bare, server_first, client_final_without_proof,
    );

    // ClientSignature = HMAC(StoredKey, AuthMessage)
    let client_signature = hmac_sha256(&stored_key, auth_message.as_bytes());

    // ClientProof = ClientKey XOR ClientSignature
    let mut proof = [0u8; 32];
    for i in 0..32 {
        proof[i] = client_key[i] ^ client_signature[i];
    }

    Ok(format!(
        "{},p={}",
        client_final_without_proof,
        BASE64.encode(proof),
    ))
}

// ── Tests ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stored_credential_from_password() {
        let cred = StoredCredential::from_password("admin", "secret");
        assert_eq!(cred.username, "admin");
        assert_eq!(cred.salt.len(), 16);
        assert_eq!(cred.iterations, DEFAULT_ITERATIONS);
        assert_ne!(cred.stored_key, [0u8; 32]);
        assert_ne!(cred.server_key, [0u8; 32]);
    }

    #[test]
    fn test_stored_credential_deterministic() {
        let salt = b"fixed_salt_16_b!";
        let c1 = StoredCredential::from_password_with_salt("u", "p", salt, 4096);
        let c2 = StoredCredential::from_password_with_salt("u", "p", salt, 4096);
        assert_eq!(c1.stored_key, c2.stored_key);
        assert_eq!(c1.server_key, c2.server_key);
    }

    #[test]
    fn test_scram_full_handshake() {
        // Server setup
        let mut store = AuthStore::new();
        store.add_user("admin", "hunter2");
        let auth_state = AuthState::new(store);

        // Client step 1: client-first
        let (client_first, client_nonce) = scram_client_first("admin");
        let client_first_bare = client_first.strip_prefix("n,,").unwrap().to_string();

        // Server step 1: begin handshake
        let (hs_token, server_first) = auth_state
            .begin_scram(&client_first)
            .expect("begin_scram should succeed");

        // Client step 2: build client-final
        let client_final =
            scram_client_final("hunter2", &client_nonce, &client_first_bare, &server_first)
                .expect("client_final should succeed");

        // Server step 2: verify and issue session
        let (session_token, server_sig) = auth_state
            .complete_scram(&hs_token, &client_final)
            .expect("complete_scram should succeed");

        assert!(
            !session_token.is_empty(),
            "session token should be non-empty"
        );
        assert!(
            !server_sig.is_empty(),
            "server signature should be non-empty"
        );

        // Session token should work
        assert!(auth_state.check_token(&session_token));
    }

    #[test]
    fn test_scram_wrong_password_rejected() {
        let mut store = AuthStore::new();
        store.add_user("admin", "correct_password");
        let auth_state = AuthState::new(store);

        let (client_first, client_nonce) = scram_client_first("admin");
        let client_first_bare = client_first.strip_prefix("n,,").unwrap().to_string();

        let (hs_token, server_first) = auth_state
            .begin_scram(&client_first)
            .expect("begin should work");

        // Use wrong password
        let client_final = scram_client_final(
            "wrong_password",
            &client_nonce,
            &client_first_bare,
            &server_first,
        )
        .expect("client_final build should work");

        let result = auth_state.complete_scram(&hs_token, &client_final);
        assert!(result.is_err(), "wrong password should be rejected");
        assert!(
            result.unwrap_err().contains("invalid client proof"),
            "error should mention invalid proof"
        );
    }

    #[test]
    fn test_scram_unknown_user_rejected() {
        let mut store = AuthStore::new();
        store.add_user("admin", "pass");
        let auth_state = AuthState::new(store);

        let (client_first, _) = scram_client_first("nonexistent");
        let result = auth_state.begin_scram(&client_first);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown user"));
    }

    #[test]
    fn test_bearer_backward_compat() {
        let mut store = AuthStore::new();
        store.set_bearer_token("my-token".to_string());
        store.add_user("admin", "pass");
        let auth_state = AuthState::new(store);

        assert!(auth_state.check_token("my-token"), "bearer should work");
        assert!(
            !auth_state.check_token("wrong-token"),
            "wrong bearer should fail"
        );
    }

    #[test]
    fn test_no_auth_configured_passes_all() {
        let store = AuthStore::new();
        assert!(!store.is_auth_required());
        let auth_state = AuthState::new(store);
        assert!(auth_state.check_token("anything"));
    }

    #[test]
    fn test_session_creation_and_expiry() {
        let mut ss = SessionStore::new(10);
        let session = ss.create_session("testuser");
        assert!(!session.is_expired());
        assert!(ss.validate_token(&session.token).is_some());
        assert!(ss.validate_token("nonexistent").is_none());
    }

    #[test]
    fn test_session_store_max_capacity() {
        let mut ss = SessionStore::new(3);
        let _s1 = ss.create_session("user1");
        let _s2 = ss.create_session("user2");
        let _s3 = ss.create_session("user3");
        assert_eq!(ss.len(), 3);

        // Adding a 4th should evict the oldest
        let _s4 = ss.create_session("user4");
        assert_eq!(ss.len(), 3, "should evict to stay at max");
    }

    #[test]
    fn test_nonce_uniqueness() {
        let n1 = generate_nonce();
        let n2 = generate_nonce();
        assert_ne!(n1, n2, "two nonces should differ");
        assert!(!n1.is_empty());
    }

    #[test]
    fn test_invalid_client_first_rejected() {
        let result = parse_client_first("garbage");
        assert!(result.is_err());

        let result = parse_client_first("n,,r=nonce_only");
        assert!(result.is_err(), "missing n= should fail");

        let result = parse_client_first("n,,n=user");
        assert!(result.is_err(), "missing r= should fail");
    }

    #[test]
    fn test_parse_client_first_valid() {
        let cf = parse_client_first("n,,n=admin,r=abc123").unwrap();
        assert_eq!(cf.username, "admin");
        assert_eq!(cf.nonce, "abc123");
        assert_eq!(cf.bare, "n=admin,r=abc123");
    }

    #[test]
    fn test_parse_client_final_valid() {
        let msg = "c=biws,r=combined_nonce,p=AAAA";
        let cf = parse_client_final(msg).unwrap();
        assert_eq!(cf.nonce, "combined_nonce");
        assert_eq!(cf.without_proof, "c=biws,r=combined_nonce");
    }

    #[test]
    fn test_handshake_token_uniqueness() {
        let t1 = generate_handshake_token();
        let t2 = generate_handshake_token();
        assert_ne!(t1, t2);
        assert_eq!(t1.len(), 32); // 16 bytes hex = 32 chars
    }

    #[test]
    fn test_auth_store_coexistence() {
        let mut store = AuthStore::new();
        store.set_bearer_token("legacy-token".to_string());
        store.add_user("admin", "scram-pass");

        assert!(store.is_auth_required());
        assert!(store.has_scram_users());
        assert!(store.check_bearer("legacy-token"));
        assert!(!store.check_bearer("wrong"));
        assert!(store.get_credential("admin").is_some());
        assert!(store.get_credential("nobody").is_none());
    }

    // ── Edge-case SCRAM tests ────────────────────────────────

    #[test]
    fn test_scram_invalid_base64_client_first() {
        // parse_client_first works on plaintext (not base64), so test invalid GS2 header
        let result = parse_client_first("x,,n=admin,r=nonce");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("GS2 header"));
    }

    #[test]
    fn test_scram_invalid_proof_base64_in_client_final() {
        // Valid structure but proof is not valid base64
        let result = parse_client_final("c=biws,r=somenonce,p=!!!not-base64!!!");
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("base64"),
            "should mention base64 decode failure"
        );
    }

    #[test]
    fn test_scram_wrong_client_proof() {
        let mut store = AuthStore::new();
        store.add_user("admin", "correct");
        let auth_state = AuthState::new(store);

        let (client_first, client_nonce) = scram_client_first("admin");
        let client_first_bare = client_first.strip_prefix("n,,").unwrap().to_string();

        let (hs_token, server_first) = auth_state.begin_scram(&client_first).unwrap();

        // Build a valid client-final but with a tampered proof (flip every byte)
        let real_final =
            scram_client_final("correct", &client_nonce, &client_first_bare, &server_first)
                .unwrap();

        // Extract the proof and corrupt it
        let p_idx = real_final.rfind(",p=").unwrap();
        let proof_b64 = &real_final[p_idx + 3..];
        let mut proof_bytes = BASE64.decode(proof_b64).unwrap();
        for b in proof_bytes.iter_mut() {
            *b ^= 0xFF; // flip all bits
        }
        let corrupted_final = format!("{},p={}", &real_final[..p_idx], BASE64.encode(&proof_bytes));

        let result = auth_state.complete_scram(&hs_token, &corrupted_final);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid client proof"));
    }

    #[test]
    fn test_scram_nonce_mismatch() {
        let mut store = AuthStore::new();
        store.add_user("admin", "pass");
        let auth_state = AuthState::new(store);

        let (client_first, _client_nonce) = scram_client_first("admin");

        let (hs_token, _server_first) = auth_state.begin_scram(&client_first).unwrap();

        // Build a client-final with a completely different nonce
        let forged_final = format!(
            "c=biws,r=completely_wrong_nonce,p={}",
            BASE64.encode([0u8; 32])
        );

        let result = auth_state.complete_scram(&hs_token, &forged_final);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("nonce mismatch"));
    }

    #[test]
    fn test_scram_empty_username() {
        let mut store = AuthStore::new();
        store.add_user("admin", "pass");
        let auth_state = AuthState::new(store);

        // Empty username in client-first: valid parse but unknown user
        let msg = "n,,n=,r=somenonce123";
        let result = auth_state.begin_scram(msg);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown user"));
    }

    #[test]
    fn test_scram_empty_password_credential() {
        // Empty password should still produce a valid credential and work in handshake
        let mut store = AuthStore::new();
        store.add_user("admin", "");
        let auth_state = AuthState::new(store);

        let (client_first, client_nonce) = scram_client_first("admin");
        let client_first_bare = client_first.strip_prefix("n,,").unwrap().to_string();

        let (hs_token, server_first) = auth_state.begin_scram(&client_first).unwrap();

        let client_final =
            scram_client_final("", &client_nonce, &client_first_bare, &server_first).unwrap();

        let result = auth_state.complete_scram(&hs_token, &client_final);
        assert!(
            result.is_ok(),
            "empty password credential should complete: {:?}",
            result
        );
    }

    #[test]
    fn test_session_store_max_capacity_evicts_oldest() {
        let mut ss = SessionStore::new(MAX_SESSIONS);
        let mut tokens = Vec::new();
        for i in 0..MAX_SESSIONS {
            let session = ss.create_session(&format!("user{}", i));
            tokens.push(session.token.clone());
        }
        assert_eq!(ss.len(), MAX_SESSIONS);

        // Add one more — should evict the oldest (first created)
        let extra = ss.create_session("overflow_user");
        assert_eq!(ss.len(), MAX_SESSIONS, "should stay at max capacity");

        // The new session should be valid
        assert!(ss.validate_token(&extra.token).is_some());

        // The first session should have been evicted
        assert!(
            ss.validate_token(&tokens[0]).is_none(),
            "oldest session should be evicted"
        );

        // A later session should still be valid
        assert!(
            ss.validate_token(&tokens[MAX_SESSIONS - 1]).is_some(),
            "recent sessions should remain"
        );
    }

    #[test]
    fn test_session_store_expired_cleanup() {
        let mut ss = SessionStore::new(100);
        let s1 = ss.create_session("user1");
        let s2 = ss.create_session("user2");
        assert_eq!(ss.len(), 2);

        // Manually insert an expired session
        let expired_token = "expired-token-abc".to_string();
        ss.sessions.insert(
            expired_token.clone(),
            AuthSession {
                token: expired_token.clone(),
                username: "expired_user".to_string(),
                expires_at: Instant::now() - std::time::Duration::from_secs(1),
            },
        );
        assert_eq!(ss.len(), 3);

        // Expired token should not validate
        assert!(ss.validate_token(&expired_token).is_none());

        // Cleanup should remove it
        ss.expire_stale();
        assert_eq!(ss.len(), 2);

        // Real sessions should still be valid
        assert!(ss.validate_token(&s1.token).is_some());
        assert!(ss.validate_token(&s2.token).is_some());
    }

    #[test]
    fn test_session_token_validation_variants() {
        let mut store = AuthStore::new();
        store.add_user("admin", "pass");
        store.set_bearer_token("bearer123".to_string());
        let auth_state = AuthState::new(store);

        // Valid bearer token
        assert!(auth_state.check_token("bearer123"));

        // Invalid random token
        assert!(!auth_state.check_token("random_garbage_token"));

        // Empty token
        assert!(!auth_state.check_token(""));

        // Perform a SCRAM handshake to get a session token
        let (client_first, client_nonce) = scram_client_first("admin");
        let client_first_bare = client_first.strip_prefix("n,,").unwrap().to_string();
        let (hs_token, server_first) = auth_state.begin_scram(&client_first).unwrap();
        let client_final =
            scram_client_final("pass", &client_nonce, &client_first_bare, &server_first).unwrap();
        let (session_token, _) = auth_state.complete_scram(&hs_token, &client_final).unwrap();

        // Session token should be valid
        assert!(auth_state.check_token(&session_token));

        // Modified session token should fail
        assert!(!auth_state.check_token(&format!("{}x", session_token)));
    }

    #[test]
    fn test_scram_concurrent_handshakes() {
        let mut store = AuthStore::new();
        store.add_user("user1", "pass1");
        store.add_user("user2", "pass2");
        store.add_user("user3", "pass3");
        let auth_state = AuthState::new(store);

        // Start 10 handshakes simultaneously (interleaved users)
        let mut handshakes = Vec::new();
        for i in 0..10 {
            let username = format!("user{}", (i % 3) + 1);
            let password = format!("pass{}", (i % 3) + 1);
            let (client_first, client_nonce) = scram_client_first(&username);
            let client_first_bare = client_first.strip_prefix("n,,").unwrap().to_string();
            let (hs_token, server_first) = auth_state.begin_scram(&client_first).unwrap();
            handshakes.push((
                hs_token,
                server_first,
                client_nonce,
                client_first_bare,
                password,
            ));
        }

        // Complete all handshakes — none should interfere with each other
        let mut session_tokens = Vec::new();
        for (hs_token, server_first, client_nonce, client_first_bare, password) in &handshakes {
            let client_final =
                scram_client_final(password, client_nonce, client_first_bare, server_first)
                    .unwrap();
            let (session_token, server_sig) =
                auth_state.complete_scram(hs_token, &client_final).unwrap();
            assert!(!session_token.is_empty());
            assert!(!server_sig.is_empty());
            session_tokens.push(session_token);
        }

        // All session tokens should be unique and valid
        for token in &session_tokens {
            assert!(auth_state.check_token(token));
        }
        let unique: std::collections::HashSet<_> = session_tokens.iter().collect();
        assert_eq!(
            unique.len(),
            session_tokens.len(),
            "all tokens must be unique"
        );
    }

    #[test]
    fn test_bearer_token_very_long() {
        let long_token: String = "A".repeat(10_240); // 10 KB
        let mut store = AuthStore::new();
        store.set_bearer_token(long_token.clone());
        let auth_state = AuthState::new(store);

        assert!(auth_state.check_token(&long_token));
        assert!(!auth_state.check_token(&"A".repeat(10_239))); // off by one
    }

    #[test]
    fn test_bearer_token_with_special_chars() {
        // Token with newlines, null bytes, unicode
        let special_token = "tok\nen\0with\u{1F600}unicode";
        let mut store = AuthStore::new();
        store.set_bearer_token(special_token.to_string());
        let auth_state = AuthState::new(store);

        assert!(auth_state.check_token(special_token));
        assert!(!auth_state.check_token("tok"));
        assert!(!auth_state.check_token(""));
    }

    #[test]
    fn test_scram_client_final_bad_proof_length() {
        // Proof that decodes to wrong number of bytes (not 32)
        let mut store = AuthStore::new();
        store.add_user("admin", "pass");
        let auth_state = AuthState::new(store);

        let (client_first, _) = scram_client_first("admin");
        let (hs_token, _server_first) = auth_state.begin_scram(&client_first).unwrap();

        // Build client-final with a proof that's only 16 bytes instead of 32
        let short_proof = BASE64.encode([0u8; 16]);
        let forged_final = format!("c=biws,r=wrongnonce,p={}", short_proof);

        let result = auth_state.complete_scram(&hs_token, &forged_final);
        assert!(result.is_err());
        // Could be nonce mismatch or proof length error — either is acceptable
    }

    #[test]
    fn test_scram_replay_handshake_token() {
        let mut store = AuthStore::new();
        store.add_user("admin", "pass");
        let auth_state = AuthState::new(store);

        let (client_first, client_nonce) = scram_client_first("admin");
        let client_first_bare = client_first.strip_prefix("n,,").unwrap().to_string();
        let (hs_token, server_first) = auth_state.begin_scram(&client_first).unwrap();

        let client_final =
            scram_client_final("pass", &client_nonce, &client_first_bare, &server_first).unwrap();

        // First completion succeeds
        let result = auth_state.complete_scram(&hs_token, &client_final);
        assert!(result.is_ok());

        // Second completion with same token fails (token consumed)
        let result2 = auth_state.complete_scram(&hs_token, &client_final);
        assert!(result2.is_err());
        assert!(result2
            .unwrap_err()
            .contains("invalid or expired handshake token"));
    }

    #[test]
    fn test_parse_client_final_missing_fields() {
        // Missing c=
        assert!(parse_client_final("r=nonce,p=AAAA").is_err());
        // Missing r=
        assert!(parse_client_final("c=biws,p=AAAA").is_err());
        // Missing p=
        assert!(parse_client_final("c=biws,r=nonce").is_err());
        // Totally empty
        assert!(parse_client_final("").is_err());
    }

    #[test]
    fn test_cleanup_expired_handshakes() {
        let store = AuthStore::new();
        let auth_state = AuthState::new(store);

        // Insert a handshake with a very old created_at
        {
            let mut hs = auth_state.handshakes.write().unwrap();
            hs.insert(
                "old-token".to_string(),
                ScramHandshake {
                    username: "test".to_string(),
                    client_nonce: "cn".to_string(),
                    server_nonce: "sn".to_string(),
                    client_first_bare: "n=test,r=cn".to_string(),
                    stored: StoredCredential::from_password("test", "pass"),
                    created_at: Instant::now() - std::time::Duration::from_secs(60),
                },
            );
        }

        auth_state.cleanup_expired();

        let hs = auth_state.handshakes.read().unwrap();
        assert!(
            hs.is_empty(),
            "expired handshake should have been cleaned up"
        );
    }
}
