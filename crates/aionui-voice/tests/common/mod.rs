//! Shared harness: in-memory DB, real auth (JWT + users), the voice router
//! with production guards, and the fake OpenAI provider.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use aionui_auth::{AuthIdentityMode, AuthState, CookieNames, JwtService, OriginPolicy};
use aionui_db::{Database, IUserRepository, SqliteUserRepository, SqliteVoiceRepository, init_database_memory};
use aionui_realtime::WebSocketManager;
use aionui_voice::fake_provider::FakeOpenAi;
use aionui_voice::{
    ApiKey, CredentialPolicy, CredentialPort, EngineTimings, OpenAiRealtimeEngine, ProviderCredential,
    SessionTokenVerifier, VoiceHttpGuards, VoiceRouterState, VoiceService, VoiceServiceDeps, VoiceSockets,
    VoiceTimings, voice_routes,
};

pub mod fakes;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response};
pub use fakes::{FakeConversations, FakeSpeech};
use http_body_util::BodyExt;
use serde_json::Value;
use tokio::net::TcpListener;
use tower::ServiceExt;

pub const ORIGIN: &str = "https://voice.test";
pub const SDP: &str = "v=0\r\no=- 4611731400430051336 2 IN IP4 127.0.0.1\r\ns=-\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n";
/// Key handed out by the fake credential port; must never appear in responses or logs.
pub const API_KEY_PREFIX: &str = "sk-test-SECRETKEY-";

#[derive(Clone)]
pub struct TestUser {
    pub id: String,
    pub token: String,
}

/// Credentials: every user except `no-provider*` has one official OpenAI row.
struct FakeCredentials;

#[async_trait::async_trait]
impl CredentialPort for FakeCredentials {
    async fn provider_credentials(&self, user_id: &str) -> Result<Vec<ProviderCredential>, String> {
        if user_id.starts_with("no-provider") {
            return Ok(vec![]);
        }
        Ok(vec![ProviderCredential {
            provider_id: format!("prov-{user_id}"),
            platform: "OpenAI".into(),
            base_url: "https://api.openai.com/v1".into(),
            enabled: true,
            api_key: ApiKey::new(format!("{API_KEY_PREFIX}{user_id}")),
        }])
    }
}

pub struct Options {
    pub timings: VoiceTimings,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            timings: fast_timings(),
        }
    }
}

pub fn fast_timings() -> VoiceTimings {
    VoiceTimings {
        engine: EngineTimings {
            attach_window: Duration::from_millis(1500),
            attach_retry_delay: Duration::from_millis(50),
            config_gate: Duration::from_millis(600),
            http_timeout: Duration::from_secs(5),
        },
        tick: Duration::from_millis(50),
        reattach_deadline: Duration::from_millis(2000),
        reattach_retry_delay: Duration::from_millis(50),
        hello_timeout: Duration::from_millis(800),
        ..VoiceTimings::default()
    }
}

pub struct Harness {
    pub conversations: Arc<FakeConversations>,
    pub speech: Arc<FakeSpeech>,
    pub db: Database,
    pub router: Router,
    pub service: VoiceService,
    pub fake: Arc<FakeOpenAi>,
    pub jwt: Arc<JwtService>,
    pub users: Arc<dyn IUserRepository>,
    pub auth_state: AuthState,
    pub voice_repo: Arc<SqliteVoiceRepository>,
}

impl Harness {
    pub async fn new(options: Options) -> Self {
        let db = init_database_memory().await.unwrap();
        let fake = Arc::new(FakeOpenAi::start().await);
        Self::with_db(db, fake, options).await
    }

    /// Build a second service over the same DB and fake (simulated restart).
    pub async fn with_db(db: Database, fake: Arc<FakeOpenAi>, options: Options) -> Self {
        let jwt = Arc::new(JwtService::new("voice-test-secret".into()));
        let users: Arc<dyn IUserRepository> = Arc::new(SqliteUserRepository::new(db.pool().clone()));
        let auth_state = AuthState {
            jwt_service: jwt.clone(),
            user_repo: users.clone(),
            identity_mode: AuthIdentityMode::UserSession,
            runtime_token_verifier: None,
            cookie_names: CookieNames::LEGACY,
        };
        let voice_repo = Arc::new(SqliteVoiceRepository::new(db.pool().clone()));
        let conversations = Arc::new(FakeConversations::default());
        let speech = Arc::new(FakeSpeech::default());
        let service = VoiceService::new(VoiceServiceDeps {
            conversations: conversations.clone(),
            drafts: voice_repo.clone(),
            speech: speech.clone(),
            settings: voice_repo.clone(),
            sessions: voice_repo.clone(),
            audit: voice_repo.clone(),
            credentials: Arc::new(FakeCredentials),
            credential_policy: CredentialPolicy::with_fake_provider_url(fake.base_url()),
            engine: Arc::new(OpenAiRealtimeEngine::new(options.timings.engine)),
            verifier: Arc::new(SessionTokenVerifier::new(auth_state.clone())),
            sockets: VoiceSockets::new(Arc::new(WebSocketManager::new())),
            timings: options.timings,
        });
        let guards = VoiceHttpGuards {
            auth_state: auth_state.clone(),
            origins: Arc::new(OriginPolicy::new(None, &[ORIGIN])),
        };
        let router = voice_routes(
            VoiceRouterState {
                service: service.clone(),
                session_cookie: "aionui-session",
            },
            guards,
        );
        Self {
            conversations,
            speech,
            db,
            router,
            service,
            fake,
            jwt,
            users,
            auth_state,
            voice_repo,
        }
    }

    pub async fn user(&self, id: &str) -> TestUser {
        sqlx::query(
            "INSERT OR IGNORE INTO users (id, user_type, username, password_hash, status, session_generation, created_at, updated_at) \
             VALUES (?, 'local', ?, 'x', 'active', 0, 1, 1)",
        )
        .bind(id)
        .bind(id)
        .execute(self.db.pool())
        .await
        .unwrap();
        let generation: (i64,) = sqlx::query_as("SELECT session_generation FROM users WHERE id = ?")
            .bind(id)
            .fetch_one(self.db.pool())
            .await
            .unwrap();
        let token = self.jwt.sign_with_session_generation(id, id, generation.0).unwrap();
        TestUser { id: id.into(), token }
    }

    pub async fn enabled_user(&self, id: &str) -> TestUser {
        let user = self.user(id).await;
        self.service
            .update_settings(
                &user.id,
                &aionui_api_types::UpdateVoiceSettingsRequest {
                    enabled: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        user
    }

    /// Revoke every session of the user (logout-everywhere / password change).
    pub async fn revoke_sessions(&self, user: &TestUser) {
        self.users.increment_session_generation(&user.id).await.unwrap();
    }

    pub async fn send(&self, request: Request<Body>) -> (u16, Value) {
        let response = self.router.clone().oneshot(request).await.unwrap();
        split(response).await
    }

    /// Serve the router on a real port (WebSocket tests).
    pub async fn serve(&self) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = self.router.clone();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        format!("127.0.0.1:{}", addr.port())
    }

    pub async fn session_row(&self, user: &TestUser, id: &str) -> aionui_db::VoiceSessionRow {
        use aionui_db::IVoiceSessionRepository;
        IVoiceSessionRepository::get(self.voice_repo.as_ref(), &user.id, id)
            .await
            .unwrap()
            .expect("session row")
    }

    /// Wait until the session row reaches a terminal state.
    pub async fn wait_ended(&self, user: &TestUser, id: &str) -> aionui_db::VoiceSessionRow {
        wait_for(Duration::from_secs(10), || async {
            let row = self.session_row(user, id).await;
            (row.state == "ended" || row.state == "failed").then_some(row)
        })
        .await
        .expect("session should end")
    }
}

pub async fn split(response: Response<Body>) -> (u16, Value) {
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// Request builder with the browser defaults: session cookie + allowed
/// Origin (no CSRF header: voice has no CSRF layer of its own). Tests remove pieces to probe each guard.
pub struct Req {
    builder: axum::http::request::Builder,
    body: Option<Value>,
}

impl Req {
    pub fn new(method: &str, uri: &str) -> Self {
        Self {
            builder: Request::builder().method(method).uri(uri),
            body: None,
        }
    }

    pub fn browser(method: &str, uri: &str, user: &TestUser) -> Self {
        Self::new(method, uri).cookie(user).header("origin", ORIGIN)
    }

    pub fn cookie(self, user: &TestUser) -> Self {
        self.header("cookie", &format!("aionui-session={}", user.token))
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.builder = self.builder.header(name, value);
        self
    }

    pub fn json(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }

    pub fn build(self) -> Request<Body> {
        match self.body {
            Some(body) => self
                .builder
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
            None => self.builder.body(Body::empty()).unwrap(),
        }
    }
}

pub async fn wait_for<F, Fut, T>(timeout: Duration, mut check: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(value) = check().await {
            return Some(value);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ----------------------------------------------------------------- WS client

pub type Client = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub async fn ws_connect(addr: &str, user: &TestUser, origin: Option<&str>) -> Result<Client, u16> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = format!("ws://{addr}/api/voice/ws").into_client_request().unwrap();
    request
        .headers_mut()
        .insert("cookie", format!("aionui-session={}", user.token).parse().unwrap());
    if let Some(origin) = origin {
        request.headers_mut().insert("origin", origin.parse().unwrap());
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok((ws, _)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => Err(response.status().as_u16()),
        Err(other) => panic!("unexpected ws error: {other}"),
    }
}

pub async fn ws_send(ws: &mut Client, frame: Value) {
    use futures_util::SinkExt;
    ws.send(tokio_tungstenite::tungstenite::Message::Text(frame.to_string().into()))
        .await
        .unwrap();
}

/// Next text frame as JSON; `None` when the socket closed or nothing arrived in time.
pub async fn ws_next(ws: &mut Client, timeout: Duration) -> Option<Value> {
    use futures_util::StreamExt;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let message = tokio::time::timeout_at(deadline, ws.next()).await.ok()??;
        match message.ok()? {
            tokio_tungstenite::tungstenite::Message::Text(text) => return serde_json::from_str(text.as_str()).ok(),
            tokio_tungstenite::tungstenite::Message::Close(_) => return None,
            _ => continue,
        }
    }
}

/// Read frames until one named `name` arrives (others are skipped).
pub async fn ws_expect(ws: &mut Client, name: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let frame = ws_next(ws, remaining)
            .await
            .unwrap_or_else(|| panic!("socket closed or timed out waiting for {name}"));
        if frame["name"] == name {
            return frame;
        }
    }
}

/// Read until the server closes; returns the frames seen before the close.
pub async fn ws_until_closed(ws: &mut Client, timeout: Duration) -> Option<Vec<Value>> {
    use futures_util::StreamExt;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut frames = Vec::new();
    loop {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Err(_) => return None,
            Ok(None) | Ok(Some(Err(_))) => return Some(frames),
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_)))) => return Some(frames),
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text)))) => {
                if let Ok(value) = serde_json::from_str(text.as_str()) {
                    frames.push(value);
                }
            }
            Ok(Some(Ok(_))) => {}
        }
    }
}

pub fn hello(session: Option<&str>) -> Value {
    serde_json::json!({"name": "voice.hello", "data": {"voiceSessionId": session, "device": "web", "capabilities": {}}})
}

pub async fn create_session(h: &Harness, user: &TestUser) -> (u16, Value) {
    h.send(
        Req::browser("POST", "/api/voice/sessions", user)
            .json(serde_json::json!({"sdp_offer": SDP, "device_id": "web"}))
            .build(),
    )
    .await
}

/// Create a session and wait until its sideband passed the config gate.
pub async fn live_session(h: &Harness, user: &TestUser) -> (String, String) {
    let (status, body) = create_session(h, user).await;
    assert_eq!(status, 201, "{body}");
    let id = body["data"]["voice_session_id"].as_str().unwrap().to_owned();
    let row = wait_for(Duration::from_secs(5), || async {
        let row = h.session_row(user, &id).await;
        row.ready_at.is_some().then_some(row)
    })
    .await
    .expect("call should become ready");
    (id, row.call_id.unwrap())
}
