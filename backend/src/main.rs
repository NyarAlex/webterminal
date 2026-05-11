use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    net::SocketAddr,
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
        Arc, Mutex,
    },
    thread,
};

use anyhow::{anyhow, Context, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tower_http::{cors::CorsLayer, services::ServeDir, trace::TraceLayer};
use tracing::{info, warn};
use uuid::Uuid;

const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 36;
const REPLAY_LIMIT_BYTES: usize = 512 * 1024;

#[derive(Clone)]
struct AppState {
    sessions: Arc<SessionRegistry>,
}

struct SessionRegistry {
    inner: Mutex<HashMap<Uuid, Arc<TerminalSession>>>,
}

impl SessionRegistry {
    fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    fn list(&self) -> Vec<SessionSummary> {
        let sessions = self.inner.lock().expect("sessions lock poisoned");
        sessions.values().map(|s| s.summary()).collect()
    }

    fn get(&self, id: &Uuid) -> Option<Arc<TerminalSession>> {
        self.inner
            .lock()
            .expect("sessions lock poisoned")
            .get(id)
            .cloned()
    }

    fn insert(&self, session: Arc<TerminalSession>) {
        self.inner
            .lock()
            .expect("sessions lock poisoned")
            .insert(session.id, session);
    }

    fn remove(&self, id: &Uuid) -> Option<Arc<TerminalSession>> {
        self.inner
            .lock()
            .expect("sessions lock poisoned")
            .remove(id)
    }
}

struct TerminalSession {
    id: Uuid,
    name: String,
    command: String,
    cleanup_command: Option<String>,
    mode: SessionMode,
    size: Mutex<PtySize>,
    created_at: DateTime<Utc>,
    input_tx: mpsc::Sender<Vec<u8>>,
    output_tx: broadcast::Sender<Vec<u8>>,
    replay: Mutex<ReplayBuffer>,
    viewers: AtomicUsize,
    pty: Mutex<Box<dyn MasterPty + Send>>,
    child: Mutex<Option<Box<dyn Child + Send + Sync>>>,
}

impl TerminalSession {
    fn summary(&self) -> SessionSummary {
        let size = self.size.lock().expect("size lock poisoned");
        SessionSummary {
            id: self.id,
            name: self.name.clone(),
            command: self.command.clone(),
            mode: self.mode,
            cols: size.cols,
            rows: size.rows,
            created_at: self.created_at,
            viewers: self.viewers.load(Ordering::Relaxed),
        }
    }

    fn write_input(&self, data: Vec<u8>) -> Result<()> {
        self.input_tx
            .send(data)
            .map_err(|_| anyhow!("terminal input channel is closed"))
    }

    fn append_output(&self, chunk: &[u8]) {
        self.replay
            .lock()
            .expect("replay lock poisoned")
            .push(chunk.to_vec());
        let _ = self.output_tx.send(chunk.to_vec());
    }

    fn replay_chunks(&self) -> Vec<Vec<u8>> {
        self.replay.lock().expect("replay lock poisoned").chunks()
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        self.pty
            .lock()
            .expect("pty lock poisoned")
            .resize(size)
            .context("failed to resize pty")?;
        *self.size.lock().expect("size lock poisoned") = size;
        Ok(())
    }

    fn close(&self) {
        if let Some(mut child) = self.child.lock().expect("child lock poisoned").take() {
            if let Err(err) = child.kill() {
                warn!(session = %self.id, error = %err, "failed to kill terminal child");
            }
        }

        if let Some(cleanup_command) = &self.cleanup_command {
            let status = Command::new("/bin/sh").arg("-lc").arg(cleanup_command).status();
            match status {
                Ok(status) if status.success() => {}
                Ok(status) => {
                    warn!(session = %self.id, status = %status, "terminal cleanup command failed");
                }
                Err(err) => {
                    warn!(session = %self.id, error = %err, "failed to run terminal cleanup command");
                }
            }
        }
    }
}

struct ReplayBuffer {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
}

impl ReplayBuffer {
    fn new() -> Self {
        Self {
            chunks: VecDeque::new(),
            bytes: 0,
        }
    }

    fn push(&mut self, chunk: Vec<u8>) {
        self.bytes += chunk.len();
        self.chunks.push_back(chunk);

        while self.bytes > REPLAY_LIMIT_BYTES {
            if let Some(old) = self.chunks.pop_front() {
                self.bytes = self.bytes.saturating_sub(old.len());
            } else {
                break;
            }
        }
    }

    fn chunks(&self) -> Vec<Vec<u8>> {
        self.chunks.iter().cloned().collect()
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum SessionMode {
    Local,
    Ssh,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CreateMode {
    Local,
    Ssh,
}

#[derive(Debug, Deserialize)]
struct CreateSessionRequest {
    name: Option<String>,
    mode: Option<CreateMode>,
    cols: Option<u16>,
    rows: Option<u16>,
    tmux_name: Option<String>,
    ssh: Option<SshTarget>,
    command: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResizeSessionRequest {
    cols: u16,
    rows: u16,
}

#[derive(Debug, Deserialize)]
struct SshTarget {
    host: String,
    username: String,
    port: Option<u16>,
    key_path: Option<String>,
}

#[derive(Debug, Serialize)]
struct SessionSummary {
    id: Uuid,
    name: String,
    command: String,
    mode: SessionMode,
    cols: u16,
    rows: u16,
    created_at: DateTime<Utc>,
    viewers: usize,
}

#[derive(Debug, Deserialize)]
struct WsParams {
    readonly: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorBody {
                error: self.0.to_string(),
            }),
        )
            .into_response()
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "backend=info,tower_http=info".into()),
        )
        .init();

    let state = AppState {
        sessions: Arc::new(SessionRegistry::new()),
    };

    let static_dir = std::env::var("WEBTERMINAL_STATIC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("../frontend/dist"));

    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/{id}", get(get_session).delete(delete_session))
        .route("/api/sessions/{id}/resize", post(resize_session))
        .route("/ws/sessions/{id}", get(ws_session))
        .fallback_service(ServeDir::new(static_dir).append_index_html_on_directories(true))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr: SocketAddr = std::env::var("WEBTERMINAL_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8787".to_string())
        .parse()
        .context("invalid WEBTERMINAL_ADDR")?;

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "webterminal backend listening");
    axum::serve(listener, app).await?;

    Ok(())
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

async fn list_sessions(State(state): State<AppState>) -> Json<Vec<SessionSummary>> {
    Json(state.sessions.list())
}

async fn get_session(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<SessionSummary>, AppError> {
    let session = state
        .sessions
        .get(&id)
        .ok_or_else(|| anyhow!("session not found"))?;

    Ok(Json(session.summary()))
}

async fn create_session(
    State(state): State<AppState>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<Json<SessionSummary>, AppError> {
    let id = Uuid::new_v4();
    let cols = req.cols.unwrap_or(DEFAULT_COLS).clamp(40, 240);
    let rows = req.rows.unwrap_or(DEFAULT_ROWS).clamp(12, 80);
    let tmux_name = sanitize_tmux_name(req.tmux_name.as_deref(), &id);

    let spec = build_command(id, &tmux_name, req)?;
    let session = spawn_session(id, spec, cols, rows)?;
    let summary = session.summary();
    state.sessions.insert(session);

    Ok(Json(summary))
}

async fn delete_session(State(state): State<AppState>, Path(id): Path<Uuid>) -> StatusCode {
    if let Some(session) = state.sessions.remove(&id) {
        session.close();
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn resize_session(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<ResizeSessionRequest>,
) -> Result<Json<SessionSummary>, AppError> {
    let session = state
        .sessions
        .get(&id)
        .ok_or_else(|| anyhow!("session not found"))?;
    let cols = req.cols.clamp(40, 240);
    let rows = req.rows.clamp(12, 80);
    session.resize(cols, rows)?;

    Ok(Json(session.summary()))
}

async fn ws_session(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(params): Query<WsParams>,
) -> Result<Response, AppError> {
    let session = state
        .sessions
        .get(&id)
        .ok_or_else(|| anyhow!("session not found"))?;
    let readonly = params.readonly.unwrap_or(false);

    Ok(ws
        .max_message_size(1024 * 1024)
        .max_frame_size(1024 * 1024)
        .on_upgrade(move |socket| handle_socket(socket, session, readonly)))
}

async fn handle_socket(mut socket: WebSocket, session: Arc<TerminalSession>, readonly: bool) {
    session.viewers.fetch_add(1, Ordering::Relaxed);
    let mut rx = session.output_tx.subscribe();

    for chunk in session.replay_chunks() {
        if socket.send(Message::Binary(chunk.into())).await.is_err() {
            session.viewers.fetch_sub(1, Ordering::Relaxed);
            return;
        }
    }

    loop {
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        if !readonly {
                            if let Err(err) = session.write_input(text.as_bytes().to_vec()) {
                                warn!(session = %session.id, error = %err, "failed to write text input");
                                break;
                            }
                        }
                    }
                    Some(Ok(Message::Binary(data))) => {
                        if !readonly {
                            if let Err(err) = session.write_input(data.to_vec()) {
                                warn!(session = %session.id, error = %err, "failed to write binary input");
                                break;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(data))) => {
                        if socket.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Err(err)) => {
                        warn!(session = %session.id, error = %err, "websocket receive failed");
                        break;
                    }
                }
            }
            output = rx.recv() => {
                match output {
                    Ok(chunk) => {
                        if socket.send(Message::Binary(chunk.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!(session = %session.id, skipped, "websocket output receiver lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    session.viewers.fetch_sub(1, Ordering::Relaxed);
}

fn spawn_session(
    id: Uuid,
    spec: SessionSpec,
    cols: u16,
    rows: u16,
) -> Result<Arc<TerminalSession>> {
    ensure_tmux_available(&spec.command)?;

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to open pty")?;

    let mut cmd = CommandBuilder::new("/bin/sh");
    cmd.arg("-lc");
    cmd.arg(&spec.command);
    cmd.env("TERM", "xterm-256color");

    let child = pair
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("failed to spawn command: {}", spec.command))?;
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .context("failed to clone pty reader")?;
    let mut writer = pair.master.take_writer().context("failed to take pty writer")?;

    let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>();
    let (output_tx, _) = broadcast::channel::<Vec<u8>>(1024);

    let session = Arc::new(TerminalSession {
        id,
        name: spec.name,
        command: spec.command,
        cleanup_command: spec.cleanup_command,
        mode: spec.mode,
        size: Mutex::new(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }),
        created_at: Utc::now(),
        input_tx,
        output_tx,
        replay: Mutex::new(ReplayBuffer::new()),
        viewers: AtomicUsize::new(0),
        pty: Mutex::new(pair.master),
        child: Mutex::new(Some(child)),
    });

    let output_session = Arc::clone(&session);
    thread::spawn(move || {
        let mut buf = [0_u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => output_session.append_output(&buf[..n]),
                Err(err) => {
                    warn!(session = %output_session.id, error = %err, "pty read failed");
                    break;
                }
            }
        }
    });

    let input_session = Arc::clone(&session);
    thread::spawn(move || {
        while let Ok(data) = input_rx.recv() {
            if let Err(err) = writer.write_all(&data) {
                warn!(session = %input_session.id, error = %err, "pty write failed");
                break;
            }
            if let Err(err) = writer.flush() {
                warn!(session = %input_session.id, error = %err, "pty flush failed");
                break;
            }
        }
    });

    info!(session = %id, "terminal session spawned");
    Ok(session)
}

struct SessionSpec {
    name: String,
    command: String,
    cleanup_command: Option<String>,
    mode: SessionMode,
}

fn build_command(id: Uuid, tmux_name: &str, req: CreateSessionRequest) -> Result<SessionSpec> {
    if let Some(command) = req.command {
        let name = req.name.unwrap_or_else(|| "Custom Command".to_string());
        return Ok(SessionSpec {
            name,
            command,
            cleanup_command: None,
            mode: SessionMode::Local,
        });
    }

    match req.mode.unwrap_or(CreateMode::Local) {
        CreateMode::Local => {
            let name = req.name.unwrap_or_else(|| format!("Local {short}", short = short_id(id)));
            let command = format!("tmux new-session -A -s {}", shell_escape(tmux_name));
            let cleanup_command = Some(format!("tmux kill-session -t {}", shell_escape(tmux_name)));
            Ok(SessionSpec {
                name,
                command,
                cleanup_command,
                mode: SessionMode::Local,
            })
        }
        CreateMode::Ssh => {
            let ssh = req.ssh.ok_or_else(|| anyhow!("ssh target is required"))?;
            let port = ssh.port.unwrap_or(22);
            let mut parts = vec![
                "ssh".to_string(),
                "-tt".to_string(),
                "-p".to_string(),
                shell_escape(&port.to_string()),
                "-o".to_string(),
                shell_escape("ServerAliveInterval=30"),
                "-o".to_string(),
                shell_escape("ServerAliveCountMax=3"),
                "-o".to_string(),
                shell_escape("StrictHostKeyChecking=accept-new"),
            ];

            if let Some(key_path) = ssh.key_path {
                parts.push("-i".to_string());
                parts.push(shell_escape(&key_path));
            }

            parts.push(shell_escape(&format!("{}@{}", ssh.username, ssh.host)));
            parts.push(shell_escape(&format!(
                "tmux new-session -A -s {}",
                shell_escape(tmux_name)
            )));

            let name = req
                .name
                .unwrap_or_else(|| format!("{}@{}", ssh.username, ssh.host));
            Ok(SessionSpec {
                name,
                command: parts.join(" "),
                cleanup_command: None,
                mode: SessionMode::Ssh,
            })
        }
    }
}

fn sanitize_tmux_name(input: Option<&str>, id: &Uuid) -> String {
    let raw = input
        .filter(|s| !s.trim().is_empty())
        .map(str::trim)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("wt-{}", short_id(*id)));

    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn short_id(id: Uuid) -> String {
    id.to_string()[..8].to_string()
}

fn shell_escape(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '@' | '='))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn ensure_tmux_available(command: &str) -> Result<()> {
    if !command.contains("tmux") {
        return Ok(());
    }

    let status = Command::new("tmux").arg("-V").status();
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(_) => Err(anyhow!("tmux exists but returned an error")),
        Err(err) => Err(anyhow!("tmux is required for terminal sessions: {err}")),
    }
}
