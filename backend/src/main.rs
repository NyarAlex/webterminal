use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    io::{Read, Write},
    net::SocketAddr,
    path::PathBuf,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
};

use anyhow::{Context, Result, anyhow};
use axum::{
    Json, Router,
    extract::{
        Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
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
    input_tx: mpsc::Sender<PtyInput>,
    input_mode: InputMode,
    output_tx: broadcast::Sender<SessionEvent>,
    replay: Mutex<ReplayBuffer>,
    viewers: AtomicUsize,
    pty: Mutex<Box<dyn MasterPty + Send>>,
    child: Mutex<Option<Box<dyn Child + Send + Sync>>>,
}

#[derive(Clone)]
enum InputMode {
    Direct,
    TmuxControl(Arc<Mutex<TmuxControlState>>),
}

enum PtyInput {
    User {
        data: Vec<u8>,
        pane_id: Option<String>,
    },
    Raw(Vec<u8>),
}

#[derive(Default)]
struct TmuxControlState {
    initialized: bool,
    active_pane: Option<String>,
    refresh_generation: u64,
    pending_generation: Option<u64>,
    pending_windows: BTreeMap<String, TmuxWindow>,
    pending_panes: BTreeMap<String, TmuxPane>,
    windows: Vec<TmuxWindow>,
    pane_replay: HashMap<String, ReplayBuffer>,
}

#[derive(Clone)]
enum SessionEvent {
    Output(Vec<u8>),
    PaneOutput { pane_id: String, data: Vec<u8> },
    TmuxState(TmuxStateSnapshot),
}

#[derive(Clone, Debug, Serialize)]
struct TmuxStateSnapshot {
    active_pane: Option<String>,
    windows: Vec<TmuxWindow>,
}

#[derive(Clone, Debug, Serialize)]
struct TmuxWindow {
    id: String,
    index: Option<u16>,
    name: String,
    active: bool,
    panes: Vec<TmuxPane>,
}

#[derive(Clone, Debug, Serialize)]
struct TmuxPane {
    id: String,
    window_id: String,
    index: Option<u16>,
    active: bool,
    current_command: String,
    current_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    FocusPane { pane_id: String },
    TmuxCommand { command: TmuxUiCommand },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TmuxUiCommand {
    NewWindow,
    SplitHorizontal,
    SplitVertical,
    KillPane,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage<'a> {
    Clear,
    FocusPane { pane_id: &'a str },
    TmuxState { state: TmuxStateSnapshot },
}

impl TmuxControlState {
    fn snapshot(&self) -> TmuxStateSnapshot {
        TmuxStateSnapshot {
            active_pane: self.active_pane.clone(),
            windows: self.windows.clone(),
        }
    }

    fn upsert_pane_placeholder(&mut self, pane_id: &str) {
        if self
            .windows
            .iter()
            .any(|window| window.panes.iter().any(|pane| pane.id == pane_id))
        {
            return;
        }

        let mut window = self.windows.first().cloned().unwrap_or_else(|| TmuxWindow {
            id: "@0".to_string(),
            index: Some(0),
            name: "tmux".to_string(),
            active: true,
            panes: Vec::new(),
        });
        window.panes.push(TmuxPane {
            id: pane_id.to_string(),
            window_id: window.id.clone(),
            index: Some(window.panes.len() as u16),
            active: true,
            current_command: String::new(),
            current_path: String::new(),
        });
        if self.windows.is_empty() {
            self.windows.push(window);
        } else {
            self.windows[0] = window;
        }
    }
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

    fn write_input(&self, data: Vec<u8>, pane_id: Option<String>) -> Result<()> {
        self.input_tx
            .send(PtyInput::User { data, pane_id })
            .map_err(|_| anyhow!("terminal input channel is closed"))
    }

    fn write_raw_input(&self, data: Vec<u8>) -> Result<()> {
        self.input_tx
            .send(PtyInput::Raw(data))
            .map_err(|_| anyhow!("terminal input channel is closed"))
    }

    fn append_direct_output(&self, chunk: &[u8]) {
        self.replay
            .lock()
            .expect("replay lock poisoned")
            .push(chunk.to_vec());
        let _ = self.output_tx.send(SessionEvent::Output(chunk.to_vec()));
    }

    fn append_pane_output(&self, pane_id: &str, chunk: &[u8]) {
        if let InputMode::TmuxControl(state) = &self.input_mode {
            state
                .lock()
                .expect("tmux control state lock poisoned")
                .pane_replay
                .entry(pane_id.to_string())
                .or_insert_with(ReplayBuffer::new)
                .push(chunk.to_vec());
        }
        let _ = self.output_tx.send(SessionEvent::PaneOutput {
            pane_id: pane_id.to_string(),
            data: chunk.to_vec(),
        });
    }

    fn replay_chunks(&self) -> Vec<Vec<u8>> {
        self.replay.lock().expect("replay lock poisoned").chunks()
    }

    fn replay_pane_chunks(&self, pane_id: &str) -> Vec<Vec<u8>> {
        match &self.input_mode {
            InputMode::TmuxControl(state) => state
                .lock()
                .expect("tmux control state lock poisoned")
                .pane_replay
                .get(pane_id)
                .map(ReplayBuffer::chunks)
                .unwrap_or_default(),
            InputMode::Direct => self.replay_chunks(),
        }
    }

    fn tmux_state_snapshot(&self) -> Option<TmuxStateSnapshot> {
        match &self.input_mode {
            InputMode::TmuxControl(state) => Some(
                state
                    .lock()
                    .expect("tmux control state lock poisoned")
                    .snapshot(),
            ),
            InputMode::Direct => None,
        }
    }

    fn default_pane_id(&self) -> Option<String> {
        self.tmux_state_snapshot().and_then(|state| {
            state.active_pane.or_else(|| {
                state
                    .windows
                    .iter()
                    .flat_map(|w| w.panes.iter())
                    .next()
                    .map(|p| p.id.clone())
            })
        })
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
        if matches!(self.input_mode, InputMode::TmuxControl(_)) {
            self.write_raw_input(format!("refresh-client -C {cols},{rows}\n").into_bytes())?;
            self.request_tmux_state_refresh()?;
        }
        Ok(())
    }

    fn request_tmux_state_refresh(&self) -> Result<()> {
        let generation = match &self.input_mode {
            InputMode::TmuxControl(state) => {
                let mut state = state.lock().expect("tmux control state lock poisoned");
                state.refresh_generation = state.refresh_generation.saturating_add(1);
                let generation = state.refresh_generation;
                state.pending_generation = Some(generation);
                state.pending_windows.clear();
                state.pending_panes.clear();
                generation
            }
            InputMode::Direct => return Ok(()),
        };

        self.write_raw_input(
            format!(
                "list-windows -F 'WT_WINDOW\t{generation}\t#{{window_id}}\t#{{window_index}}\t#{{window_name}}\t#{{window_active}}'\n\
                 list-panes -s -F 'WT_PANE\t{generation}\t#{{window_id}}\t#{{pane_id}}\t#{{pane_index}}\t#{{pane_active}}\t#{{pane_current_command}}\t#{{pane_current_path}}'\n\
                 display-message -p 'WT_DONE\t{generation}'\n"
            )
            .into_bytes(),
        )
    }

    fn send_tmux_ui_command(&self, command: TmuxUiCommand, pane_id: Option<&str>) -> Result<()> {
        let target = pane_id.map(tmux_quote);
        let command = match command {
            TmuxUiCommand::NewWindow => "new-window\n".to_string(),
            TmuxUiCommand::SplitHorizontal => {
                format!("split-window -h{}\n", tmux_target_arg(target.as_deref()))
            }
            TmuxUiCommand::SplitVertical => {
                format!("split-window -v{}\n", tmux_target_arg(target.as_deref()))
            }
            TmuxUiCommand::KillPane => {
                format!("kill-pane{}\n", tmux_target_arg(target.as_deref()))
            }
        };
        self.write_raw_input(command.into_bytes())?;
        self.request_tmux_state_refresh()
    }

    fn close(&self) {
        if let Some(mut child) = self.child.lock().expect("child lock poisoned").take() {
            if let Err(err) = child.kill() {
                warn!(session = %self.id, error = %err, "failed to kill terminal child");
            }
        }

        if let Some(cleanup_command) = &self.cleanup_command {
            let status = Command::new("/bin/sh")
                .arg("-lc")
                .arg(cleanup_command)
                .status();
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
    LocalCc,
    SshCc,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CreateMode {
    Local,
    Ssh,
    LocalCc,
    SshCc,
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
        .route(
            "/api/sessions/{id}",
            get(get_session).delete(delete_session),
        )
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
    let mut focused_pane = session.default_pane_id();

    if let Some(state) = session.tmux_state_snapshot() {
        if send_server_message(&mut socket, &ServerMessage::TmuxState { state })
            .await
            .is_err()
        {
            session.viewers.fetch_sub(1, Ordering::Relaxed);
            return;
        }
        if let Some(pane_id) = focused_pane.as_deref() {
            if send_focus_and_replay(&mut socket, &session, pane_id)
                .await
                .is_err()
            {
                session.viewers.fetch_sub(1, Ordering::Relaxed);
                return;
            }
        } else if let Err(err) = session.request_tmux_state_refresh() {
            warn!(session = %session.id, error = %err, "failed to request tmux state");
        }
    } else {
        for chunk in session.replay_chunks() {
            if socket.send(Message::Binary(chunk.into())).await.is_err() {
                session.viewers.fetch_sub(1, Ordering::Relaxed);
                return;
            }
        }
    }

    loop {
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        if readonly {
                            continue;
                        }
                        match serde_json::from_str::<ClientMessage>(&text) {
                            Ok(ClientMessage::FocusPane { pane_id }) => {
                                focused_pane = Some(pane_id.clone());
                                if send_focus_and_replay(&mut socket, &session, &pane_id).await.is_err() {
                                    break;
                                }
                            }
                            Ok(ClientMessage::TmuxCommand { command }) => {
                                if let Err(err) = session.send_tmux_ui_command(command, focused_pane.as_deref()) {
                                    warn!(session = %session.id, error = %err, "failed to send tmux UI command");
                                    break;
                                }
                            }
                            Err(_) => {
                                if let Err(err) = session.write_input(text.as_bytes().to_vec(), focused_pane.clone()) {
                                    warn!(session = %session.id, error = %err, "failed to write text input");
                                    break;
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Binary(data))) => {
                        if !readonly {
                            if let Err(err) = session.write_input(data.to_vec(), focused_pane.clone()) {
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
                    Ok(SessionEvent::Output(chunk)) => {
                        if socket.send(Message::Binary(chunk.into())).await.is_err() { break; }
                    }
                    Ok(SessionEvent::PaneOutput { pane_id, data }) => {
                        if focused_pane.as_deref() == Some(pane_id.as_str())
                            && socket.send(Message::Binary(data.into())).await.is_err() {
                                break;
                            }
                    }
                    Ok(SessionEvent::TmuxState(state)) => {
                        if focused_pane.is_none() {
                            focused_pane = state.active_pane.clone().or_else(|| {
                                state.windows.iter().flat_map(|window| window.panes.iter()).next().map(|pane| pane.id.clone())
                            });
                            if let Some(pane_id) = focused_pane.as_deref()
                                && send_focus_and_replay(&mut socket, &session, pane_id).await.is_err() {
                                    break;
                                }
                        }
                        if send_server_message(&mut socket, &ServerMessage::TmuxState { state }).await.is_err() {
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

async fn send_focus_and_replay(
    socket: &mut WebSocket,
    session: &TerminalSession,
    pane_id: &str,
) -> Result<()> {
    send_server_message(socket, &ServerMessage::FocusPane { pane_id }).await?;
    send_server_message(socket, &ServerMessage::Clear).await?;
    for chunk in session.replay_pane_chunks(pane_id) {
        socket
            .send(Message::Binary(chunk.into()))
            .await
            .context("failed to send pane replay")?;
    }
    Ok(())
}

async fn send_server_message(socket: &mut WebSocket, message: &ServerMessage<'_>) -> Result<()> {
    socket
        .send(Message::Text(serde_json::to_string(message)?.into()))
        .await
        .context("failed to send websocket message")
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

    let reader = pair
        .master
        .try_clone_reader()
        .context("failed to clone pty reader")?;
    let mut writer = pair
        .master
        .take_writer()
        .context("failed to take pty writer")?;

    let (input_tx, input_rx) = mpsc::channel::<PtyInput>();
    let (output_tx, _) = broadcast::channel::<SessionEvent>(1024);

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
        input_mode: spec.input_mode.clone(),
        output_tx,
        replay: Mutex::new(ReplayBuffer::new()),
        viewers: AtomicUsize::new(0),
        pty: Mutex::new(pair.master),
        child: Mutex::new(Some(child)),
    });

    let output_session = Arc::clone(&session);
    let output_mode = spec.input_mode.clone();
    thread::spawn(move || match output_mode {
        InputMode::Direct => read_direct_output(output_session, reader),
        InputMode::TmuxControl(state) => read_tmux_control_output(output_session, reader, state),
    });

    let input_session = Arc::clone(&session);
    let input_mode = session.input_mode.clone();
    thread::spawn(move || {
        while let Ok(input) = input_rx.recv() {
            let payload = match input {
                PtyInput::User { data, pane_id } => {
                    encode_input_for_mode(&input_mode, &data, pane_id.as_deref())
                }
                PtyInput::Raw(data) => data,
            };
            if payload.is_empty() {
                continue;
            }
            if let Err(err) = writer.write_all(&payload) {
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

fn read_direct_output(session: Arc<TerminalSession>, mut reader: Box<dyn Read + Send>) {
    let mut buf = [0_u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => session.append_direct_output(&buf[..n]),
            Err(err) => {
                warn!(session = %session.id, error = %err, "pty read failed");
                break;
            }
        }
    }
}

fn read_tmux_control_output(
    session: Arc<TerminalSession>,
    mut reader: Box<dyn Read + Send>,
    state: Arc<Mutex<TmuxControlState>>,
) {
    let mut buf = [0_u8; 8192];
    let mut pending = Vec::<u8>::new();

    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                pending.extend_from_slice(&buf[..n]);
                flush_tmux_startup_passthrough(&session, &state, &mut pending);
                while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
                    let mut line = pending.drain(..=pos).collect::<Vec<u8>>();
                    if line.ends_with(b"\n") {
                        line.pop();
                    }
                    if line.ends_with(b"\r") {
                        line.pop();
                    }
                    handle_tmux_control_line(&session, &state, &line);
                }
            }
            Err(err) => {
                warn!(session = %session.id, error = %err, "tmux control pty read failed");
                break;
            }
        }
    }
}

fn flush_tmux_startup_passthrough(
    session: &Arc<TerminalSession>,
    state: &Arc<Mutex<TmuxControlState>>,
    pending: &mut Vec<u8>,
) {
    let initialized = state
        .lock()
        .expect("tmux control state lock poisoned")
        .initialized;
    if initialized || pending.is_empty() || pending[0] == b'%' {
        return;
    }

    let flush_len = pending
        .iter()
        .position(|b| *b == b'%')
        .unwrap_or(pending.len());
    let raw_chunk = pending.drain(..flush_len).collect::<Vec<_>>();
    let chunk = strip_tmux_control_wrappers(&raw_chunk);
    if !chunk.is_empty() {
        session.append_direct_output(&chunk);
    }
}

fn strip_tmux_control_wrappers(input: &[u8]) -> Vec<u8> {
    const CONTROL_START: &[u8] = b"\x1bP1000p";
    const CONTROL_END: &[u8] = b"\x1b\\";

    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;

    while index < input.len() {
        if input[index..].starts_with(CONTROL_START) {
            index += CONTROL_START.len();
        } else if input[index..].starts_with(CONTROL_END) {
            index += CONTROL_END.len();
        } else {
            output.push(input[index]);
            index += 1;
        }
    }

    output
}

fn handle_tmux_control_line(
    session: &Arc<TerminalSession>,
    state: &Arc<Mutex<TmuxControlState>>,
    line: &[u8],
) {
    if line.is_empty() {
        return;
    }

    if !line.starts_with(b"%") {
        if handle_tmux_state_line(session, state, line) {
            return;
        }
        let initialized = state
            .lock()
            .expect("tmux control state lock poisoned")
            .initialized;
        if !initialized {
            session.append_direct_output(line);
            session.append_direct_output(b"\r\n");
        }
        return;
    }

    let was_initialized = {
        let mut state = state.lock().expect("tmux control state lock poisoned");
        let was_initialized = state.initialized;
        state.initialized = true;
        was_initialized
    };
    if !was_initialized {
        if let Err(err) = session.request_tmux_state_refresh() {
            warn!(session = %session.id, error = %err, "failed to request tmux state");
        }
    }

    if let Some((pane, payload)) = parse_tmux_output_line(line) {
        {
            let mut state = state.lock().expect("tmux control state lock poisoned");
            if state.active_pane.is_none() {
                state.active_pane = Some(pane.clone());
            }
            state.upsert_pane_placeholder(&pane);
        }
        let decoded = decode_tmux_escaped_bytes(payload);
        if !decoded.is_empty() {
            session.append_pane_output(&pane, &decoded);
        }
        broadcast_tmux_state(session);
        return;
    }

    if is_tmux_structure_event(line) {
        if let Err(err) = session.request_tmux_state_refresh() {
            warn!(session = %session.id, error = %err, "failed to refresh tmux state after event");
        }
    }
}

fn handle_tmux_state_line(
    session: &Arc<TerminalSession>,
    state: &Arc<Mutex<TmuxControlState>>,
    line: &[u8],
) -> bool {
    let Ok(text) = std::str::from_utf8(line) else {
        return false;
    };
    if let Some(rest) = text.strip_prefix("WT_WINDOW\t") {
        let parts = rest.splitn(5, '\t').collect::<Vec<_>>();
        if parts.len() == 5 {
            let generation = parts[0].parse::<u64>().ok();
            let mut state = state.lock().expect("tmux control state lock poisoned");
            if state.pending_generation == generation {
                state.pending_windows.insert(
                    parts[1].to_string(),
                    TmuxWindow {
                        id: parts[1].to_string(),
                        index: parts[2].parse::<u16>().ok(),
                        name: parts[3].to_string(),
                        active: parts[4] == "1",
                        panes: Vec::new(),
                    },
                );
            }
        }
        return true;
    }

    if let Some(rest) = text.strip_prefix("WT_PANE\t") {
        let parts = rest.splitn(7, '\t').collect::<Vec<_>>();
        if parts.len() == 7 {
            let generation = parts[0].parse::<u64>().ok();
            let mut state = state.lock().expect("tmux control state lock poisoned");
            if state.pending_generation == generation {
                let pane = TmuxPane {
                    window_id: parts[1].to_string(),
                    id: parts[2].to_string(),
                    index: parts[3].parse::<u16>().ok(),
                    active: parts[4] == "1",
                    current_command: parts[5].to_string(),
                    current_path: parts[6].to_string(),
                };
                if pane.active {
                    state.active_pane = Some(pane.id.clone());
                }
                state.pending_panes.insert(pane.id.clone(), pane);
            }
        }
        return true;
    }

    if let Some(rest) = text.strip_prefix("WT_DONE\t") {
        let generation = rest.trim().parse::<u64>().ok();
        let snapshot = {
            let mut state = state.lock().expect("tmux control state lock poisoned");
            if state.pending_generation == generation {
                let mut windows = std::mem::take(&mut state.pending_windows);
                for pane in std::mem::take(&mut state.pending_panes).into_values() {
                    windows
                        .entry(pane.window_id.clone())
                        .or_insert_with(|| TmuxWindow {
                            id: pane.window_id.clone(),
                            index: None,
                            name: pane.window_id.clone(),
                            active: false,
                            panes: Vec::new(),
                        })
                        .panes
                        .push(pane);
                }
                state.windows = windows.into_values().collect();
                for window in &mut state.windows {
                    window
                        .panes
                        .sort_by_key(|pane| pane.index.unwrap_or(u16::MAX));
                }
                state
                    .windows
                    .sort_by_key(|window| window.index.unwrap_or(u16::MAX));
                state.pending_generation = None;
                Some(state.snapshot())
            } else {
                None
            }
        };
        if let Some(snapshot) = snapshot {
            let _ = session.output_tx.send(SessionEvent::TmuxState(snapshot));
        }
        return true;
    }

    false
}

fn broadcast_tmux_state(session: &TerminalSession) {
    if let Some(state) = session.tmux_state_snapshot() {
        let _ = session.output_tx.send(SessionEvent::TmuxState(state));
    }
}

fn is_tmux_structure_event(line: &[u8]) -> bool {
    line.starts_with(b"%window-")
        || line.starts_with(b"%layout-change")
        || line.starts_with(b"%pane-")
        || line.starts_with(b"%session-")
}

fn parse_tmux_output_line(line: &[u8]) -> Option<(String, &[u8])> {
    if let Some(rest) = line.strip_prefix(b"%output ") {
        let space = rest.iter().position(|b| *b == b' ')?;
        let pane = String::from_utf8_lossy(&rest[..space]).to_string();
        return Some((pane, &rest[space + 1..]));
    }

    if let Some(rest) = line.strip_prefix(b"%extended-output ") {
        let pane_end = rest.iter().position(|b| *b == b' ')?;
        let pane = String::from_utf8_lossy(&rest[..pane_end]).to_string();
        let colon = rest.windows(2).position(|window| window == b": ")?;
        return Some((pane, &rest[colon + 2..]));
    }

    None
}

fn decode_tmux_escaped_bytes(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;

    while index < input.len() {
        if input[index] == b'\\' {
            if index + 3 < input.len()
                && input[index + 1].is_ascii_digit()
                && input[index + 2].is_ascii_digit()
                && input[index + 3].is_ascii_digit()
                && input[index + 1] < b'8'
                && input[index + 2] < b'8'
                && input[index + 3] < b'8'
            {
                let value = (input[index + 1] - b'0') * 64
                    + (input[index + 2] - b'0') * 8
                    + (input[index + 3] - b'0');
                output.push(value);
                index += 4;
                continue;
            }

            if index + 1 < input.len() {
                output.push(input[index + 1]);
                index += 2;
                continue;
            }
        }

        output.push(input[index]);
        index += 1;
    }

    output
}

fn encode_input_for_mode(mode: &InputMode, data: &[u8], pane_override: Option<&str>) -> Vec<u8> {
    match mode {
        InputMode::Direct => data.to_vec(),
        InputMode::TmuxControl(state) => {
            let pane = {
                let state = state.lock().expect("tmux control state lock poisoned");
                if !state.initialized {
                    return data.to_vec();
                }
                state.active_pane.clone()
            };

            if let Some(pane) = pane_override.map(ToOwned::to_owned).or(pane) {
                encode_tmux_control_input(&pane, data)
            } else {
                data.to_vec()
            }
        }
    }
}

fn encode_tmux_control_input(pane: &str, data: &[u8]) -> Vec<u8> {
    let mut commands = String::new();
    let mut literal = String::new();
    let text = String::from_utf8_lossy(data);
    let chars = text.chars().collect::<Vec<_>>();
    let mut index = 0;

    while index < chars.len() {
        if chars[index] == '\u{1b}' && index + 2 < chars.len() && chars[index + 1] == '[' {
            flush_tmux_literal(&mut commands, pane, &mut literal);

            if let Some(final_offset) = chars[index + 2..]
                .iter()
                .position(|c| matches!(*c, '@'..='~'))
            {
                let final_index = index + 2 + final_offset;
                let params = chars[index + 2..final_index].iter().collect::<String>();
                match (params.as_str(), chars[final_index]) {
                    ("", 'A') => commands.push_str(&format!("send-keys -t {pane} Up\n")),
                    ("", 'B') => commands.push_str(&format!("send-keys -t {pane} Down\n")),
                    ("", 'C') => commands.push_str(&format!("send-keys -t {pane} Right\n")),
                    ("", 'D') => commands.push_str(&format!("send-keys -t {pane} Left\n")),
                    ("", 'H') | ("1", '~') | ("7", '~') => {
                        commands.push_str(&format!("send-keys -t {pane} Home\n"));
                    }
                    ("", 'F') | ("4", '~') | ("8", '~') => {
                        commands.push_str(&format!("send-keys -t {pane} End\n"));
                    }
                    ("3", '~') => commands.push_str(&format!("send-keys -t {pane} DC\n")),
                    ("5", '~') => commands.push_str(&format!("send-keys -t {pane} PPage\n")),
                    ("6", '~') => commands.push_str(&format!("send-keys -t {pane} NPage\n")),
                    ("200", '~') | ("201", '~') => {}
                    _ => {}
                }
                index = final_index + 1;
                continue;
            }

            commands.push_str(&format!("send-keys -t {pane} Escape\n"));
            index += 1;
            continue;
        }

        match chars[index] {
            '\r' | '\n' => {
                flush_tmux_literal(&mut commands, pane, &mut literal);
                commands.push_str(&format!("send-keys -t {pane} Enter\n"));
            }
            '\t' => {
                flush_tmux_literal(&mut commands, pane, &mut literal);
                commands.push_str(&format!("send-keys -t {pane} Tab\n"));
            }
            '\u{7f}' | '\u{8}' => {
                flush_tmux_literal(&mut commands, pane, &mut literal);
                commands.push_str(&format!("send-keys -t {pane} BSpace\n"));
            }
            '\u{3}' => {
                flush_tmux_literal(&mut commands, pane, &mut literal);
                commands.push_str(&format!("send-keys -t {pane} C-c\n"));
            }
            '\u{4}' => {
                flush_tmux_literal(&mut commands, pane, &mut literal);
                commands.push_str(&format!("send-keys -t {pane} C-d\n"));
            }
            '\u{1b}' => {
                flush_tmux_literal(&mut commands, pane, &mut literal);
                commands.push_str(&format!("send-keys -t {pane} Escape\n"));
            }
            c => literal.push(c),
        }
        index += 1;
    }

    flush_tmux_literal(&mut commands, pane, &mut literal);
    commands.into_bytes()
}

fn flush_tmux_literal(commands: &mut String, pane: &str, literal: &mut String) {
    if literal.is_empty() {
        return;
    }
    commands.push_str(&format!(
        "send-keys -t {pane} -l -- {}\n",
        tmux_quote(literal)
    ));
    literal.clear();
}

fn tmux_quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "'\\''"))
}

struct SessionSpec {
    name: String,
    command: String,
    cleanup_command: Option<String>,
    mode: SessionMode,
    input_mode: InputMode,
}

fn build_command(id: Uuid, tmux_name: &str, req: CreateSessionRequest) -> Result<SessionSpec> {
    if let Some(command) = req.command {
        let name = req.name.unwrap_or_else(|| "Custom Command".to_string());
        return Ok(SessionSpec {
            name,
            command,
            cleanup_command: None,
            mode: SessionMode::Local,
            input_mode: InputMode::Direct,
        });
    }

    match req.mode.unwrap_or(CreateMode::LocalCc) {
        CreateMode::Local => {
            let name = req
                .name
                .unwrap_or_else(|| format!("Local {short}", short = short_id(id)));
            let command = format!("tmux new-session -A -s {}", shell_escape(tmux_name));
            let cleanup_command = Some(format!("tmux kill-session -t {}", shell_escape(tmux_name)));
            Ok(SessionSpec {
                name,
                command,
                cleanup_command,
                mode: SessionMode::Local,
                input_mode: InputMode::Direct,
            })
        }
        CreateMode::LocalCc => {
            let name = req
                .name
                .unwrap_or_else(|| format!("Local CC {short}", short = short_id(id)));
            let command = format!("tmux -CC new-session -A -s {}", shell_escape(tmux_name));
            let cleanup_command = Some(format!("tmux kill-session -t {}", shell_escape(tmux_name)));
            Ok(SessionSpec {
                name,
                command,
                cleanup_command,
                mode: SessionMode::LocalCc,
                input_mode: InputMode::TmuxControl(Arc::new(Mutex::new(
                    TmuxControlState::default(),
                ))),
            })
        }
        CreateMode::Ssh => {
            let ssh = req.ssh.ok_or_else(|| anyhow!("ssh target is required"))?;
            let port = ssh.port.unwrap_or(22);
            let parts = ssh_command_parts(
                &ssh,
                port,
                &format!("tmux new-session -A -s {}", shell_escape(tmux_name)),
            );

            let name = req
                .name
                .unwrap_or_else(|| format!("{}@{}", ssh.username, ssh.host));
            Ok(SessionSpec {
                name,
                command: parts.join(" "),
                cleanup_command: None,
                mode: SessionMode::Ssh,
                input_mode: InputMode::Direct,
            })
        }
        CreateMode::SshCc => {
            let ssh = req.ssh.ok_or_else(|| anyhow!("ssh target is required"))?;
            let port = ssh.port.unwrap_or(22);
            let parts = ssh_command_parts(
                &ssh,
                port,
                &format!("tmux -CC new-session -A -s {}", shell_escape(tmux_name)),
            );

            let name = req
                .name
                .unwrap_or_else(|| format!("{}@{} CC", ssh.username, ssh.host));
            Ok(SessionSpec {
                name,
                command: parts.join(" "),
                cleanup_command: None,
                mode: SessionMode::SshCc,
                input_mode: InputMode::TmuxControl(Arc::new(Mutex::new(
                    TmuxControlState::default(),
                ))),
            })
        }
    }
}

fn ssh_command_parts(ssh: &SshTarget, port: u16, remote_command: &str) -> Vec<String> {
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

    if let Some(key_path) = &ssh.key_path {
        parts.push("-i".to_string());
        parts.push(shell_escape(key_path));
    }

    parts.push(shell_escape(&format!("{}@{}", ssh.username, ssh.host)));
    parts.push(shell_escape(remote_command));
    parts
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

fn tmux_target_arg(target: Option<&str>) -> String {
    target
        .map(|target| format!(" -t {target}"))
        .unwrap_or_default()
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
