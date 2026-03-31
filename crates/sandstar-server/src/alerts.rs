//! Email alert system for channel fault/down notifications.
//!
//! Monitors channel status transitions and sends alerts when channels
//! enter Fault or Down status. Sends recovery notifications when channels
//! return to Ok. Includes cooldown to prevent spam and an in-memory
//! history ring buffer.
//!
//! The actual email sending is behind a pluggable `AlertSender` trait,
//! allowing tests to run without SMTP and production to swap in `lettre`
//! or any other transport.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use sandstar_ipc::types::ChannelInfo;

// ── Configuration ──────────────────────────────────────────

/// Alert system configuration, loaded from a JSON file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertConfig {
    /// Master enable/disable for the alert system.
    pub enabled: bool,
    /// SMTP server hostname.
    pub smtp_host: String,
    /// SMTP server port (typically 587 for STARTTLS).
    pub smtp_port: u16,
    /// SMTP username for authentication.
    pub smtp_user: String,
    /// SMTP password (app-specific password recommended).
    #[serde(default)]
    pub smtp_password: String,
    /// Email "From" address.
    pub from_address: String,
    /// List of recipient email addresses.
    pub recipients: Vec<String>,
    /// Minimum minutes between re-alerting for the same channel.
    pub cooldown_minutes: u64,
    /// Whether to alert on Fault status transitions.
    pub alert_on_fault: bool,
    /// Whether to alert on Down status transitions.
    pub alert_on_down: bool,
    /// Whether to send recovery notifications.
    pub alert_on_recovery: bool,
    /// Subject line prefix for alert emails.
    pub subject_prefix: String,
}

impl Default for AlertConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            smtp_host: "smtp.gmail.com".into(),
            smtp_port: 587,
            smtp_user: String::new(),
            smtp_password: String::new(),
            from_address: String::new(),
            recipients: Vec::new(),
            cooldown_minutes: 15,
            alert_on_fault: true,
            alert_on_down: true,
            alert_on_recovery: true,
            subject_prefix: "[Sandstar Alert]".into(),
        }
    }
}

impl AlertConfig {
    /// Load configuration from a JSON file.
    pub fn load(path: &Path) -> Result<Self, String> {
        let data = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read alert config {}: {}", path.display(), e))?;
        serde_json::from_str(&data)
            .map_err(|e| format!("failed to parse alert config: {}", e))
    }

    /// Save configuration to a JSON file.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("failed to serialize alert config: {}", e))?;
        std::fs::write(path, json)
            .map_err(|e| format!("failed to write alert config {}: {}", path.display(), e))
    }

    /// Return a copy with the SMTP password masked for API responses.
    pub fn masked(&self) -> Self {
        let mut copy = self.clone();
        if !copy.smtp_password.is_empty() {
            copy.smtp_password = "********".into();
        }
        copy
    }
}

// ── Alert types ────────────────────────────────────────────

/// The kind of alert event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertType {
    Fault,
    Down,
    Recovery,
}

impl AlertType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fault => "FAULT",
            Self::Down => "DOWN",
            Self::Recovery => "RECOVERED",
        }
    }
}

/// Tracks the alert state for a single channel that is currently in fault/down.
#[derive(Debug, Clone)]
struct ActiveAlert {
    channel_id: u32,
    channel_name: String,
    status: String,
    value: f64,
    since: Instant,
}

/// An entry in the alert history ring buffer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertHistoryEntry {
    pub timestamp: String,
    pub channel_id: u32,
    pub channel_name: String,
    pub alert_type: AlertType,
    pub status: String,
    pub value: f64,
    pub recipients_notified: usize,
}

/// Information about an active (unresolved) alert, for the REST API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveAlertInfo {
    pub channel_id: u32,
    pub channel_name: String,
    pub status: String,
    pub value: f64,
    pub since_secs: u64,
}

// ── Alert sender trait ─────────────────────────────────────

/// Pluggable email sender. Implement this trait to provide actual SMTP
/// transport (e.g., via `lettre`). The default `LogSender` just logs
/// alert details via `tracing::warn!`.
pub trait AlertSender: Send + Sync {
    /// Send an email with the given subject and body to all recipients.
    /// Returns the number of recipients successfully notified.
    fn send(&self, config: &AlertConfig, subject: &str, body: &str) -> usize;
}

/// Default sender that logs alerts but does not actually send email.
#[derive(Debug, Default)]
pub struct LogSender;

impl AlertSender for LogSender {
    fn send(&self, config: &AlertConfig, subject: &str, body: &str) -> usize {
        warn!(
            recipients = config.recipients.len(),
            subject = %subject,
            "ALERT (email not configured, logging only): {}",
            body,
        );
        0
    }
}

// ── AlertManager ───────────────────────────────────────────

/// Maximum number of history entries to keep.
const MAX_HISTORY: usize = 100;

/// Thread-safe alert manager, wrapped in `Arc<Mutex<>>` for shared access
/// between the poll loop (status checks) and REST handlers (config/history).
pub type SharedAlertManager = Arc<Mutex<AlertManager>>;

/// Core alert manager that tracks channel status transitions and
/// dispatches notifications.
pub struct AlertManager {
    config: AlertConfig,
    config_path: Option<std::path::PathBuf>,
    /// Channels currently in alert state (channel_id -> ActiveAlert).
    active_alerts: HashMap<u32, ActiveAlert>,
    /// Last alert sent time per channel (for cooldown enforcement).
    last_alert_time: HashMap<u32, Instant>,
    /// Recent alert history (ring buffer, newest at back).
    history: VecDeque<AlertHistoryEntry>,
    /// Pluggable email sender.
    sender: Box<dyn AlertSender>,
    /// Device info string for alert emails.
    device_info: String,
}

impl AlertManager {
    /// Create a new AlertManager with the given config and default LogSender.
    pub fn new(config: AlertConfig) -> Self {
        Self {
            config,
            config_path: None,
            active_alerts: HashMap::new(),
            last_alert_time: HashMap::new(),
            history: VecDeque::with_capacity(MAX_HISTORY),
            sender: Box::new(LogSender),
            device_info: format!(
                "Sandstar Engine v{} (unknown host)",
                env!("CARGO_PKG_VERSION")
            ),
        }
    }

    /// Load an AlertManager from a JSON config file. Falls back to disabled
    /// defaults if the file doesn't exist (not an error — alerts are optional).
    pub fn load(path: &Path) -> Self {
        let config = if path.exists() {
            match AlertConfig::load(path) {
                Ok(c) => {
                    info!(path = %path.display(), "loaded alert config");
                    c
                }
                Err(e) => {
                    warn!("failed to load alert config, alerts disabled: {}", e);
                    AlertConfig::default()
                }
            }
        } else {
            info!(path = %path.display(), "no alert config found, alerts disabled");
            AlertConfig::default()
        };
        let mut mgr = Self::new(config);
        mgr.config_path = Some(path.to_path_buf());
        mgr
    }

    /// Set the device info string used in alert emails.
    pub fn set_device_info(&mut self, info: String) {
        self.device_info = info;
    }

    /// Replace the alert sender (e.g., with an SMTP implementation).
    pub fn set_sender(&mut self, sender: Box<dyn AlertSender>) {
        self.sender = sender;
    }

    /// Get a reference to the current configuration.
    pub fn config(&self) -> &AlertConfig {
        &self.config
    }

    /// Update the configuration. Optionally persists to disk if a config
    /// path was set during load.
    pub fn update_config(&mut self, new_config: AlertConfig) -> Result<(), String> {
        if let Some(ref path) = self.config_path {
            new_config.save(path)?;
        }
        self.config = new_config;
        Ok(())
    }

    /// Add a recipient. Returns true if the recipient was newly added.
    pub fn add_recipient(&mut self, email: &str) -> Result<bool, String> {
        if self.config.recipients.iter().any(|r| r == email) {
            return Ok(false);
        }
        self.config.recipients.push(email.to_string());
        if let Some(ref path) = self.config_path {
            self.config.save(path)?;
        }
        Ok(true)
    }

    /// Remove a recipient. Returns true if the recipient was found and removed.
    pub fn remove_recipient(&mut self, email: &str) -> Result<bool, String> {
        let before = self.config.recipients.len();
        self.config.recipients.retain(|r| r != email);
        let removed = self.config.recipients.len() < before;
        if removed {
            if let Some(ref path) = self.config_path {
                self.config.save(path)?;
            }
        }
        Ok(removed)
    }

    /// Get current active alerts for the REST API.
    pub fn active_alerts(&self) -> Vec<ActiveAlertInfo> {
        self.active_alerts
            .values()
            .map(|a| ActiveAlertInfo {
                channel_id: a.channel_id,
                channel_name: a.channel_name.clone(),
                status: a.status.clone(),
                value: a.value,
                since_secs: a.since.elapsed().as_secs(),
            })
            .collect()
    }

    /// Get alert history entries (newest first).
    pub fn history(&self) -> Vec<AlertHistoryEntry> {
        self.history.iter().rev().cloned().collect()
    }

    /// Send a test email to verify SMTP configuration.
    pub fn send_test(&self) -> usize {
        let subject = format!("{} Test Alert", self.config.subject_prefix);
        let body = format!(
            "SANDSTAR ENGINE TEST ALERT\n\n\
             This is a test alert to verify your SMTP configuration.\n\
             Device: {}\n\n\
             If you received this email, your alert system is working correctly.\n\
             To manage recipients, use the API: GET /api/alerts/recipients\n",
            self.device_info,
        );
        self.sender.send(&self.config, &subject, &body)
    }

    /// Scan a list of channels for status transitions and dispatch alerts.
    ///
    /// Call this after each poll cycle with the current channel snapshot.
    /// Detects:
    /// - Ok -> Fault/Down: sends alert (respecting cooldown)
    /// - Fault/Down -> Ok: sends recovery notification
    /// - Fault/Down -> Fault/Down (same): no re-alert within cooldown
    pub fn check_channels(&mut self, channels: &[ChannelSnapshot]) {
        if !self.config.enabled {
            return;
        }

        let now = Instant::now();
        let cooldown = std::time::Duration::from_secs(self.config.cooldown_minutes * 60);
        let timestamp = now_timestamp();

        // Collect IDs of channels still in alert state
        let mut still_alerting: Vec<u32> = Vec::new();

        for ch in channels {
            let is_fault = ch.status == "Fault";
            let is_down = ch.status == "Down";
            let is_alert_status = (is_fault && self.config.alert_on_fault)
                || (is_down && self.config.alert_on_down);

            if is_alert_status {
                still_alerting.push(ch.id);

                if self.active_alerts.contains_key(&ch.id) {
                    // Already tracking this channel — skip (cooldown handled below)
                    continue;
                }

                // New transition to Fault/Down
                let alert_type = if is_fault {
                    AlertType::Fault
                } else {
                    AlertType::Down
                };

                // Check cooldown
                if let Some(last) = self.last_alert_time.get(&ch.id) {
                    if now.duration_since(*last) < cooldown {
                        // Within cooldown, still register as active but don't send
                        self.active_alerts.insert(
                            ch.id,
                            ActiveAlert {
                                channel_id: ch.id,
                                channel_name: ch.name.clone(),
                                status: ch.status.clone(),
                                value: ch.cur,
                                since: now,
                            },
                        );
                        continue;
                    }
                }

                // Send alert
                let notified = self.dispatch_alert(alert_type, ch, &timestamp);

                self.active_alerts.insert(
                    ch.id,
                    ActiveAlert {
                        channel_id: ch.id,
                        channel_name: ch.name.clone(),
                        status: ch.status.clone(),
                        value: ch.cur,
                        since: now,
                    },
                );
                self.last_alert_time.insert(ch.id, now);

                self.push_history(AlertHistoryEntry {
                    timestamp: timestamp.clone(),
                    channel_id: ch.id,
                    channel_name: ch.name.clone(),
                    alert_type,
                    status: ch.status.clone(),
                    value: ch.cur,
                    recipients_notified: notified,
                });
            }
        }

        // Check for recoveries: channels that were in active_alerts but are now Ok
        let recovered: Vec<u32> = self
            .active_alerts
            .keys()
            .copied()
            .filter(|id| !still_alerting.contains(id))
            .collect();

        for ch_id in recovered {
            if let Some(alert) = self.active_alerts.remove(&ch_id) {
                if self.config.alert_on_recovery {
                    // Find the current channel info for the recovery value
                    let (cur_val, cur_status) = channels
                        .iter()
                        .find(|c| c.id == ch_id)
                        .map(|c| (c.cur, c.status.clone()))
                        .unwrap_or((0.0, "Ok".into()));

                    let snapshot = ChannelSnapshot {
                        id: ch_id,
                        name: alert.channel_name.clone(),
                        status: cur_status.clone(),
                        cur: cur_val,
                    };

                    let notified = self.dispatch_recovery(&snapshot, &timestamp);

                    self.push_history(AlertHistoryEntry {
                        timestamp: timestamp.clone(),
                        channel_id: ch_id,
                        channel_name: alert.channel_name,
                        alert_type: AlertType::Recovery,
                        status: cur_status,
                        value: cur_val,
                        recipients_notified: notified,
                    });
                }
            }
        }
    }

    /// Format and send an alert email.
    fn dispatch_alert(
        &self,
        alert_type: AlertType,
        ch: &ChannelSnapshot,
        timestamp: &str,
    ) -> usize {
        let subject = format!(
            "{} {}: Channel {} ({})",
            self.config.subject_prefix,
            alert_type.as_str(),
            ch.id,
            ch.name,
        );
        let body = format!(
            "SANDSTAR ENGINE ALERT\n\n\
             Status:    {}\n\
             Channel:   {} — {}\n\
             Value:     {:.2}\n\
             Time:      {}\n\
             Device:    {}\n\n\
             This alert was generated automatically.\n\
             To manage recipients, use the API: GET /api/alerts/recipients\n",
            ch.status, ch.id, ch.name, ch.cur, timestamp, self.device_info,
        );
        self.sender.send(&self.config, &subject, &body)
    }

    /// Format and send a recovery email.
    fn dispatch_recovery(&self, ch: &ChannelSnapshot, timestamp: &str) -> usize {
        let subject = format!(
            "{} RECOVERED: Channel {} ({})",
            self.config.subject_prefix, ch.id, ch.name,
        );
        let body = format!(
            "SANDSTAR ENGINE ALERT — RECOVERY\n\n\
             Status:    {} (was alerting)\n\
             Channel:   {} — {}\n\
             Value:     {:.2}\n\
             Time:      {}\n\
             Device:    {}\n\n\
             Channel has returned to normal operation.\n\
             To manage recipients, use the API: GET /api/alerts/recipients\n",
            ch.status, ch.id, ch.name, ch.cur, timestamp, self.device_info,
        );
        self.sender.send(&self.config, &subject, &body)
    }

    fn push_history(&mut self, entry: AlertHistoryEntry) {
        if self.history.len() >= MAX_HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(entry);
    }
}

// ── Channel snapshot for alert checking ────────────────────

/// Lightweight channel snapshot used by the alert system.
/// Decoupled from ChannelInfo to avoid pulling in all IPC types.
#[derive(Debug, Clone)]
pub struct ChannelSnapshot {
    pub id: u32,
    pub name: String,
    pub status: String,
    pub cur: f64,
}

impl From<&ChannelInfo> for ChannelSnapshot {
    fn from(ch: &ChannelInfo) -> Self {
        Self {
            id: ch.id,
            name: ch.label.clone(),
            status: ch.status.clone(),
            cur: ch.cur,
        }
    }
}

// ── REST API handlers ──────────────────────────────────────

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};

/// Build the alert management router.
pub fn alert_router(manager: SharedAlertManager) -> Router {
    Router::new()
        .route("/api/alerts/config", get(get_config))
        .route("/api/alerts/config", put(put_config))
        .route("/api/alerts/recipients", get(get_recipients))
        .route("/api/alerts/recipients", post(add_recipient))
        .route("/api/alerts/recipients/{email}", delete(remove_recipient))
        .route("/api/alerts/active", get(get_active))
        .route("/api/alerts/history", get(get_history))
        .route("/api/alerts/test", post(send_test))
        .with_state(manager)
}

/// GET /api/alerts/config — view current alert config (password masked).
async fn get_config(
    State(mgr): State<SharedAlertManager>,
) -> impl IntoResponse {
    let mgr = mgr.lock().unwrap();
    Json(mgr.config().masked())
}

/// PUT /api/alerts/config — update the alert configuration.
async fn put_config(
    State(mgr): State<SharedAlertManager>,
    Json(new_config): Json<AlertConfig>,
) -> impl IntoResponse {
    let mut mgr = mgr.lock().unwrap();
    match mgr.update_config(new_config) {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        ),
    }
}

/// GET /api/alerts/recipients — list recipient email addresses.
async fn get_recipients(
    State(mgr): State<SharedAlertManager>,
) -> impl IntoResponse {
    let mgr = mgr.lock().unwrap();
    Json(mgr.config().recipients.clone())
}

#[derive(Deserialize)]
struct AddRecipientRequest {
    email: String,
}

/// POST /api/alerts/recipients — add a recipient email address.
async fn add_recipient(
    State(mgr): State<SharedAlertManager>,
    Json(req): Json<AddRecipientRequest>,
) -> impl IntoResponse {
    let mut mgr = mgr.lock().unwrap();
    match mgr.add_recipient(&req.email) {
        Ok(true) => (
            StatusCode::CREATED,
            Json(serde_json::json!({"added": true, "email": req.email})),
        ),
        Ok(false) => (
            StatusCode::OK,
            Json(serde_json::json!({"added": false, "email": req.email, "reason": "already exists"})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        ),
    }
}

/// DELETE /api/alerts/recipients/{email} — remove a recipient.
async fn remove_recipient(
    State(mgr): State<SharedAlertManager>,
    AxumPath(email): AxumPath<String>,
) -> impl IntoResponse {
    let mut mgr = mgr.lock().unwrap();
    match mgr.remove_recipient(&email) {
        Ok(true) => (
            StatusCode::OK,
            Json(serde_json::json!({"removed": true, "email": email})),
        ),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"removed": false, "email": email, "reason": "not found"})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        ),
    }
}

/// GET /api/alerts/active — list currently active (unresolved) alerts.
async fn get_active(
    State(mgr): State<SharedAlertManager>,
) -> impl IntoResponse {
    let mgr = mgr.lock().unwrap();
    Json(mgr.active_alerts())
}

/// GET /api/alerts/history — recent alert history (newest first, max 100).
async fn get_history(
    State(mgr): State<SharedAlertManager>,
) -> impl IntoResponse {
    let mgr = mgr.lock().unwrap();
    Json(mgr.history())
}

/// POST /api/alerts/test — send a test email.
async fn send_test(
    State(mgr): State<SharedAlertManager>,
) -> impl IntoResponse {
    let mgr = mgr.lock().unwrap();
    let notified = mgr.send_test();
    Json(serde_json::json!({
        "test_sent": true,
        "recipients_notified": notified,
    }))
}

// ── Helpers ────────────────────────────────────────────────

/// Get current UTC timestamp as a string.
fn now_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Simple epoch-based timestamp (no chrono dependency)
    format_epoch(secs)
}

/// Format epoch seconds as "YYYY-MM-DD HH:MM:SS UTC" without external deps.
fn format_epoch(epoch_secs: u64) -> String {
    // Simple date formatting using basic arithmetic
    let secs = epoch_secs;
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Compute year/month/day from days since epoch (1970-01-01)
    let (year, month, day) = days_to_ymd(days);

    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
        year, month, day, hours, minutes, seconds,
    )
}

/// Convert days since 1970-01-01 to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Civil calendar algorithm
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ── Tests ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A test sender that records sent messages.
    #[derive(Default)]
    struct MockSender {
        sent: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl AlertSender for MockSender {
        fn send(&self, _config: &AlertConfig, subject: &str, body: &str) -> usize {
            self.sent
                .lock()
                .unwrap()
                .push((subject.to_string(), body.to_string()));
            2 // pretend we notified 2 recipients
        }
    }

    fn test_config() -> AlertConfig {
        AlertConfig {
            enabled: true,
            cooldown_minutes: 15,
            alert_on_fault: true,
            alert_on_down: true,
            alert_on_recovery: true,
            subject_prefix: "[Test]".into(),
            recipients: vec!["admin@test.com".into(), "tech@test.com".into()],
            ..Default::default()
        }
    }

    fn make_snapshot(id: u32, name: &str, status: &str, cur: f64) -> ChannelSnapshot {
        ChannelSnapshot {
            id,
            name: name.into(),
            status: status.into(),
            cur,
        }
    }

    #[test]
    fn config_load_save_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alerts.json");
        let config = test_config();
        config.save(&path).unwrap();
        let loaded = AlertConfig::load(&path).unwrap();
        assert_eq!(loaded.enabled, config.enabled);
        assert_eq!(loaded.cooldown_minutes, config.cooldown_minutes);
        assert_eq!(loaded.recipients.len(), 2);
        assert_eq!(loaded.subject_prefix, "[Test]");
    }

    #[test]
    fn config_masked_hides_password() {
        let mut config = test_config();
        config.smtp_password = "secret123".into();
        let masked = config.masked();
        assert_eq!(masked.smtp_password, "********");
        // Original unchanged
        assert_eq!(config.smtp_password, "secret123");
    }

    #[test]
    fn transition_ok_to_fault_sends_alert() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sender = MockSender {
            sent: sent.clone(),
        };
        let mut mgr = AlertManager::new(test_config());
        mgr.set_sender(Box::new(sender));

        let channels = vec![
            make_snapshot(1113, "Zone Temp", "Fault", -40.0),
            make_snapshot(1200, "Pressure", "Ok", 25.0),
        ];
        mgr.check_channels(&channels);

        let messages = sent.lock().unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].0.contains("FAULT"));
        assert!(messages[0].0.contains("1113"));

        // Should have 1 active alert
        assert_eq!(mgr.active_alerts().len(), 1);
        assert_eq!(mgr.active_alerts()[0].channel_id, 1113);

        // History should have 1 entry
        assert_eq!(mgr.history().len(), 1);
        assert_eq!(mgr.history()[0].alert_type, AlertType::Fault);
    }

    #[test]
    fn no_realert_within_cooldown() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sender = MockSender {
            sent: sent.clone(),
        };
        let mut mgr = AlertManager::new(test_config());
        mgr.set_sender(Box::new(sender));

        let channels = vec![make_snapshot(1113, "Zone Temp", "Fault", -40.0)];

        // First check: sends alert
        mgr.check_channels(&channels);
        assert_eq!(sent.lock().unwrap().len(), 1);

        // Simulate channel returning to Ok briefly
        mgr.active_alerts.remove(&1113);

        // Channel goes fault again within cooldown
        mgr.check_channels(&channels);

        // Should NOT have sent a second alert (cooldown)
        assert_eq!(sent.lock().unwrap().len(), 1);
        // But should still track as active
        assert_eq!(mgr.active_alerts().len(), 1);
    }

    #[test]
    fn recovery_sends_notification() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sender = MockSender {
            sent: sent.clone(),
        };
        let mut mgr = AlertManager::new(test_config());
        mgr.set_sender(Box::new(sender));

        // Fault first
        let fault_channels = vec![make_snapshot(1113, "Zone Temp", "Fault", -40.0)];
        mgr.check_channels(&fault_channels);
        assert_eq!(sent.lock().unwrap().len(), 1);

        // Then recovery
        let ok_channels = vec![make_snapshot(1113, "Zone Temp", "Ok", 72.5)];
        mgr.check_channels(&ok_channels);

        let messages = sent.lock().unwrap();
        assert_eq!(messages.len(), 2);
        assert!(messages[1].0.contains("RECOVERED"));
        assert!(messages[1].0.contains("1113"));

        // No more active alerts
        assert_eq!(mgr.active_alerts().len(), 0);

        // History should have 2 entries
        assert_eq!(mgr.history().len(), 2);
    }

    #[test]
    fn disabled_config_skips_all() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sender = MockSender {
            sent: sent.clone(),
        };
        let mut config = test_config();
        config.enabled = false;
        let mut mgr = AlertManager::new(config);
        mgr.set_sender(Box::new(sender));

        let channels = vec![make_snapshot(1113, "Zone Temp", "Fault", -40.0)];
        mgr.check_channels(&channels);

        assert_eq!(sent.lock().unwrap().len(), 0);
        assert_eq!(mgr.active_alerts().len(), 0);
    }

    #[test]
    fn alert_on_down_only() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sender = MockSender {
            sent: sent.clone(),
        };
        let mut config = test_config();
        config.alert_on_fault = false;
        config.alert_on_down = true;
        let mut mgr = AlertManager::new(config);
        mgr.set_sender(Box::new(sender));

        let channels = vec![
            make_snapshot(1113, "Zone Temp", "Fault", -40.0),
            make_snapshot(1200, "Pressure", "Down", 0.0),
        ];
        mgr.check_channels(&channels);

        let messages = sent.lock().unwrap();
        // Only Down should trigger, not Fault
        assert_eq!(messages.len(), 1);
        assert!(messages[0].0.contains("DOWN"));
        assert!(messages[0].0.contains("1200"));
    }

    #[test]
    fn recipient_add_remove() {
        let mut mgr = AlertManager::new(test_config());

        // Add new recipient
        assert!(mgr.add_recipient("new@test.com").unwrap());
        assert_eq!(mgr.config().recipients.len(), 3);

        // Duplicate returns false
        assert!(!mgr.add_recipient("new@test.com").unwrap());
        assert_eq!(mgr.config().recipients.len(), 3);

        // Remove
        assert!(mgr.remove_recipient("new@test.com").unwrap());
        assert_eq!(mgr.config().recipients.len(), 2);

        // Remove non-existent returns false
        assert!(!mgr.remove_recipient("nobody@test.com").unwrap());
    }

    #[test]
    fn history_ring_buffer_caps_at_max() {
        let mut mgr = AlertManager::new(test_config());

        for i in 0..(MAX_HISTORY + 10) {
            mgr.push_history(AlertHistoryEntry {
                timestamp: format!("t{}", i),
                channel_id: i as u32,
                channel_name: format!("ch{}", i),
                alert_type: AlertType::Fault,
                status: "Fault".into(),
                value: 0.0,
                recipients_notified: 0,
            });
        }

        assert_eq!(mgr.history.len(), MAX_HISTORY);
        // Oldest entries should have been dropped
        assert_eq!(mgr.history.front().unwrap().timestamp, "t10");
    }

    #[test]
    fn multiple_channels_alert_independently() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sender = MockSender {
            sent: sent.clone(),
        };
        let mut mgr = AlertManager::new(test_config());
        mgr.set_sender(Box::new(sender));

        let channels = vec![
            make_snapshot(1113, "Zone Temp", "Fault", -40.0),
            make_snapshot(1200, "Pressure", "Down", 0.0),
            make_snapshot(1300, "Humidity", "Ok", 45.0),
        ];
        mgr.check_channels(&channels);

        assert_eq!(sent.lock().unwrap().len(), 2);
        assert_eq!(mgr.active_alerts().len(), 2);

        // Recover one
        let channels2 = vec![
            make_snapshot(1113, "Zone Temp", "Ok", 72.0),
            make_snapshot(1200, "Pressure", "Down", 0.0),
            make_snapshot(1300, "Humidity", "Ok", 45.0),
        ];
        mgr.check_channels(&channels2);

        // 2 alerts + 1 recovery = 3 total
        assert_eq!(sent.lock().unwrap().len(), 3);
        assert_eq!(mgr.active_alerts().len(), 1);
        assert_eq!(mgr.active_alerts()[0].channel_id, 1200);
    }

    #[test]
    fn load_missing_file_returns_disabled() {
        let mgr = AlertManager::load(Path::new("/nonexistent/alerts.json"));
        assert!(!mgr.config().enabled);
    }

    #[test]
    fn format_epoch_basic() {
        // 2026-01-01 00:00:00 UTC = 1767225600
        let s = format_epoch(1767225600);
        assert!(s.contains("2026"), "expected 2026 in '{}'", s);
        assert!(s.contains("UTC"));
    }

    #[test]
    fn channel_snapshot_from_channel_info() {
        let info = ChannelInfo {
            id: 1113,
            label: "Zone Temp".into(),
            channel_type: "Analog".into(),
            direction: "In".into(),
            enabled: true,
            status: "Fault".into(),
            cur: -40.0,
            raw: 0.0,
        };
        let snap = ChannelSnapshot::from(&info);
        assert_eq!(snap.id, 1113);
        assert_eq!(snap.name, "Zone Temp");
        assert_eq!(snap.status, "Fault");
        assert_eq!(snap.cur, -40.0);
    }

    #[test]
    fn persistent_fault_no_repeat_alert() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sender = MockSender {
            sent: sent.clone(),
        };
        let mut mgr = AlertManager::new(test_config());
        mgr.set_sender(Box::new(sender));

        let channels = vec![make_snapshot(1113, "Zone Temp", "Fault", -40.0)];

        // Check multiple times — channel stays in Fault
        for _ in 0..5 {
            mgr.check_channels(&channels);
        }

        // Should only have sent 1 alert total
        assert_eq!(sent.lock().unwrap().len(), 1);
    }
}
