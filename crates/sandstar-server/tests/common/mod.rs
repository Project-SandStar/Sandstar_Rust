//! Test harness: spins up a Sandstar REST API server with MockHal for integration tests.

use std::collections::HashMap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use sandstar_engine::channel::{Channel, ChannelDirection, ChannelType};
use sandstar_engine::value::ValueConv;
use sandstar_engine::Engine;
use sandstar_hal::mock::MockHal;
use sandstar_server::auth::AuthStore;
use sandstar_server::cmd_handler::{self, CmdContext, WatchState};
use sandstar_server::config::ServerConfig;
use sandstar_server::history::{HistoryPoint, HistoryStore};
use sandstar_server::rest::{self, EngineCmd, EngineHandle};
use tokio::sync::mpsc;

/// A running test server: REST API backed by MockHal engine.
pub struct TestServer {
    pub base_url: String,
    /// Keep the background task alive.
    _cmd_task: tokio::task::JoinHandle<()>,
}

impl TestServer {
    /// Spin up a test server with default demo channels.
    pub async fn start() -> Self {
        Self::start_full(setup_demo_engine(), false, None).await
    }

    /// Spin up a test server in read-only mode.
    pub async fn start_read_only() -> Self {
        Self::start_full(setup_demo_engine(), true, None).await
    }

    /// Spin up a test server with a custom engine.
    pub async fn start_with(engine: Engine<MockHal>) -> Self {
        Self::start_full(engine, false, None).await
    }

    /// Spin up a test server with a custom engine and optional read-only mode.
    pub async fn start_with_config(engine: Engine<MockHal>, read_only: bool) -> Self {
        Self::start_full(engine, read_only, None).await
    }

    /// Spin up a test server with auth token enabled on protected routes.
    pub async fn start_with_auth(token: &str) -> Self {
        Self::start_full(setup_demo_engine(), false, Some(token.to_string())).await
    }

    /// Spin up a test server with SCRAM-SHA-256 auth (+ optional bearer token).
    pub async fn start_with_scram(
        username: &str,
        password: &str,
        bearer_token: Option<&str>,
    ) -> Self {
        use sandstar_server::auth::AuthState;

        let mut auth_store = AuthStore::new();
        auth_store.add_user(username, password);
        if let Some(bt) = bearer_token {
            auth_store.set_bearer_token(bt.to_string());
        }
        let auth_state = AuthState::new(auth_store);

        let engine = setup_demo_engine();
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCmd>(64);
        let handle = EngineHandle::new(cmd_tx);
        let app = rest::router_with_auth(handle, auth_state, bearer_token.map(|s| s.to_string()), 0);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let cmd_task = tokio::spawn(cmd_loop(engine, cmd_rx, false));

        TestServer {
            base_url,
            _cmd_task: cmd_task,
        }
    }

    /// Spin up a test server with rate limiting enabled.
    pub async fn start_with_rate_limit(rate_limit: u64) -> Self {
        let engine = setup_demo_engine();
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCmd>(64);
        let handle = EngineHandle::new(cmd_tx);
        let app = rest::router(handle, None, rate_limit);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let cmd_task = tokio::spawn(cmd_loop(engine, cmd_rx, false));

        TestServer {
            base_url,
            _cmd_task: cmd_task,
        }
    }

    /// Internal: spin up a test server with all options.
    async fn start_full(
        engine: Engine<MockHal>,
        read_only: bool,
        auth_token: Option<String>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCmd>(64);
        let handle = EngineHandle::new(cmd_tx);
        let app = rest::router(handle, auth_token, 0);

        // Bind to port 0 for auto-assignment
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());

        // Spawn HTTP server
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // Spawn command loop (engine thread)
        let cmd_task = tokio::spawn(cmd_loop(engine, cmd_rx, read_only));

        TestServer {
            base_url,
            _cmd_task: cmd_task,
        }
    }

    /// Build a GET URL.
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Build a WebSocket URL.
    pub fn ws_url(&self, path: &str) -> String {
        format!("{}{}", self.base_url.replace("http://", "ws://"), path)
    }
}

/// Engine command loop — processes REST commands against the engine.
async fn cmd_loop(mut engine: Engine<MockHal>, mut cmd_rx: mpsc::Receiver<EngineCmd>, read_only: bool) {
    let config = ServerConfig {
        socket_path: String::new(),
        poll_interval_ms: 1000,
        config_dir: None,
        read_only,
        auth_store: AuthStore::new(),
        auth_token: None,
        rate_limit: 0,
    };
    let start_time = Instant::now();
    let mut watches: HashMap<String, WatchState> = HashMap::new();
    let mut watch_counter: u64 = 0;
    let mut history_store = HistoryStore::new(100);

    while let Some(cmd) = cmd_rx.recv().await {
        // Detect PollNow before dispatching (cmd is consumed by handle_engine_cmd)
        let is_poll_now = matches!(&cmd, EngineCmd::PollNow { .. });

        let mut ctx = CmdContext {
            config: &config,
            start_time,
            watches: &mut watches,
            watch_counter: &mut watch_counter,
            history_store: &history_store,
        };
        cmd_handler::handle_engine_cmd(cmd, &mut engine, &mut ctx);

        // After PollNow, record all polled values into history (mirrors main.rs behavior)
        if is_poll_now {
            let now_unix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            for (&ch_id, item) in engine.polls.iter() {
                history_store.record(ch_id, HistoryPoint {
                    ts: now_unix,
                    cur: item.last_value.cur,
                    raw: item.last_value.raw,
                    status: item.last_value.status,
                });
            }
        }
    }
}

/// Create a demo engine with 5 channels matching main.rs demo mode.
pub fn setup_demo_engine() -> Engine<MockHal> {
    let hal = MockHal::new();
    let mut engine = Engine::new(hal);

    let channels = [
        (1113, ChannelType::Analog, ChannelDirection::In, 0, 0, "AI1 Thermistor 10K"),
        (1200, ChannelType::Analog, ChannelDirection::In, 0, 1, "AI2 0-10V"),
        (612, ChannelType::I2c, ChannelDirection::In, 2, 0x40, "I2C SDP610 CFM"),
        (2001, ChannelType::Digital, ChannelDirection::Out, 0, 47, "DO1 Relay"),
        (2002, ChannelType::Pwm, ChannelDirection::Out, 0, 0, "PWM1 Fan Speed"),
    ];

    for (id, ct, dir, dev, addr, label) in channels {
        let ch = Channel::new(id, ct, dir, dev, addr, false, ValueConv::default(), label);
        let _ = engine.channels.add(ch);
        if !dir.is_output() {
            let _ = engine.polls.add(id);
        }
    }

    engine.hal.set_analog(0, 0, Ok(2048.0));
    engine.hal.set_analog(0, 1, Ok(3276.0));
    engine.hal.set_i2c(2, 0x40, "I2C SDP610 CFM", Ok(500.0));

    engine
}
