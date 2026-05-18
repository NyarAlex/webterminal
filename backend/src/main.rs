use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fs,
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
use tempfile::NamedTempFile;
use tokio::sync::broadcast;
use tower_http::{cors::CorsLayer, services::ServeDir, trace::TraceLayer};
use tracing::{info, warn};
use uuid::Uuid;

const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 36;
const REPLAY_LIMIT_BYTES: usize = 32 * 1024 * 1024;
const INITIAL_REPLAY_LIMIT_BYTES: usize = 4 * 1024 * 1024;
const CAPTURE_PANE_HISTORY_LINES: u16 = 5000;
const FILE_TRANSFER_BASE64_LIMIT: usize = 64 * 1024 * 1024;
const DIRECT_STREAM_ID: &str = "__direct__";
const DOWNLOAD_BEGIN_MARKER: &str = "__WEBTERMINAL_DOWNLOAD_BEGIN__:";
const DOWNLOAD_END_MARKER: &str = "__WEBTERMINAL_DOWNLOAD_END__:";
const DOWNLOAD_ERROR_MARKER: &str = "__WEBTERMINAL_DOWNLOAD_ERROR__:";

#[derive(Clone)]
struct AppState {
    sessions: Arc<SessionRegistry>,
    store: Arc<SessionStore>,
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

    fn replace(&self, id: Uuid, session: Arc<TerminalSession>) {
        self.inner
            .lock()
            .expect("sessions lock poisoned")
            .insert(id, session);
    }
}

struct TerminalSession {
    id: Uuid,
    name: String,
    command: String,
    cleanup_command: Option<String>,
    persistent: Option<PersistedSession>,
    mode: SessionMode,
    size: Mutex<PtySize>,
    created_at: DateTime<Utc>,
    input_tx: mpsc::Sender<PtyInput>,
    input_mode: InputMode,
    output_tx: broadcast::Sender<SessionEvent>,
    replay: Mutex<ReplayBuffer>,
    file_transfers: Mutex<FileTransferFilter>,
    viewers: AtomicUsize,
    alive: std::sync::atomic::AtomicBool,
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
    FileTransfer {
        data: Vec<u8>,
        pane_id: Option<String>,
    },
    Paste {
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
    pending_capture_queue: VecDeque<String>,
    active_capture: Option<TmuxCapture>,
    windows: Vec<TmuxWindow>,
    pane_replay: HashMap<String, ReplayBuffer>,
    pane_notes: HashMap<String, String>,
}

#[derive(Default)]
struct TmuxCapture {
    pane_id: String,
    lines: Vec<Vec<u8>>,
}

#[derive(Clone)]
enum SessionEvent {
    Output(Vec<u8>),
    PaneOutput {
        pane_id: String,
        data: Vec<u8>,
    },
    TmuxState(TmuxStateSnapshot),
    FileDownload {
        id: String,
        filename_base64: String,
        data_base64: String,
    },
    FileTransferStatus {
        id: String,
        status: String,
        message: String,
    },
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
    zoomed: bool,
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
    note: Option<String>,
    left: Option<u16>,
    top: Option<u16>,
    width: Option<u16>,
    height: Option<u16>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    FocusPane {
        pane_id: String,
    },
    TmuxCommand {
        command: TmuxUiCommand,
        pane_id: Option<String>,
    },
    Paste {
        data: String,
        pane_id: Option<String>,
    },
    ResizePane {
        pane_id: String,
        direction: TmuxResizeDirection,
        amount: u16,
    },
    RenameWindow {
        window_id: String,
        name: String,
    },
    SetPaneNote {
        pane_id: String,
        note: String,
    },
    FileDownload {
        id: String,
        path: String,
        pane_id: Option<String>,
    },
    FileUploadStart {
        id: String,
        path: String,
        pane_id: Option<String>,
    },
    FileUploadChunk {
        id: String,
        data: String,
    },
    FileUploadFinish {
        id: String,
    },
    FileTransferCancel {
        id: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TmuxUiCommand {
    NewWindow,
    SplitHorizontal,
    SplitVertical,
    KillPane,
    ZoomPane,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TmuxResizeDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage<'a> {
    Clear,
    FocusPane {
        pane_id: &'a str,
    },
    TmuxState {
        state: TmuxStateSnapshot,
    },
    FileDownload {
        id: &'a str,
        filename_base64: &'a str,
        data_base64: &'a str,
    },
    FileTransferStatus {
        id: &'a str,
        status: &'a str,
        message: &'a str,
    },
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
            zoomed: false,
            panes: Vec::new(),
        });
        window.panes.push(TmuxPane {
            id: pane_id.to_string(),
            window_id: window.id.clone(),
            index: Some(window.panes.len() as u16),
            active: true,
            current_command: String::new(),
            current_path: String::new(),
            note: self.pane_notes.get(pane_id).cloned(),
            left: None,
            top: None,
            width: None,
            height: None,
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
            alive: self.is_alive(),
        }
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    fn write_input(&self, data: Vec<u8>, pane_id: Option<String>) -> Result<()> {
        self.input_tx
            .send(PtyInput::User { data, pane_id })
            .map_err(|_| anyhow!("terminal input channel is closed"))
    }

    fn write_file_transfer_input(&self, data: Vec<u8>, pane_id: Option<String>) -> Result<()> {
        self.input_tx
            .send(PtyInput::FileTransfer { data, pane_id })
            .map_err(|_| anyhow!("terminal input channel is closed"))
    }

    fn write_paste(&self, data: Vec<u8>, pane_id: Option<String>) -> Result<()> {
        self.input_tx
            .send(PtyInput::Paste { data, pane_id })
            .map_err(|_| anyhow!("terminal input channel is closed"))
    }

    fn write_raw_input(&self, data: Vec<u8>) -> Result<()> {
        self.input_tx
            .send(PtyInput::Raw(data))
            .map_err(|_| anyhow!("terminal input channel is closed"))
    }

    fn append_direct_output(&self, chunk: &[u8]) {
        let (visible, events) = self.filter_file_transfer_output(DIRECT_STREAM_ID, chunk);
        for chunk in visible {
            self.replay
                .lock()
                .expect("replay lock poisoned")
                .push(chunk.clone());
            let _ = self.output_tx.send(SessionEvent::Output(chunk));
        }
        self.broadcast_file_transfer_events(events);
    }

    fn append_pane_output(&self, pane_id: &str, chunk: &[u8]) {
        let (visible, events) = self.filter_file_transfer_output(pane_id, chunk);
        if let InputMode::TmuxControl(state) = &self.input_mode {
            let mut state = state.lock().expect("tmux control state lock poisoned");
            let replay = state
                .pane_replay
                .entry(pane_id.to_string())
                .or_insert_with(ReplayBuffer::new);
            for chunk in &visible {
                replay.push(chunk.clone());
            }
        }
        for chunk in visible {
            let _ = self.output_tx.send(SessionEvent::PaneOutput {
                pane_id: pane_id.to_string(),
                data: chunk,
            });
        }
        self.broadcast_file_transfer_events(events);
    }

    fn replace_pane_snapshot(&self, pane_id: &str, snapshot: Vec<u8>) {
        if let InputMode::TmuxControl(state) = &self.input_mode {
            let mut state = state.lock().expect("tmux control state lock poisoned");
            let replay = state
                .pane_replay
                .entry(pane_id.to_string())
                .or_insert_with(ReplayBuffer::new);
            replay.replace(snapshot);
        }
    }

    fn replay_tail_chunks(&self) -> Vec<Vec<u8>> {
        self.replay
            .lock()
            .expect("replay lock poisoned")
            .tail_chunks(INITIAL_REPLAY_LIMIT_BYTES)
    }

    fn replay_pane_tail_chunks(&self, pane_id: &str) -> Vec<Vec<u8>> {
        match &self.input_mode {
            InputMode::TmuxControl(state) => state
                .lock()
                .expect("tmux control state lock poisoned")
                .pane_replay
                .get(pane_id)
                .map(|replay| replay.tail_chunks(INITIAL_REPLAY_LIMIT_BYTES))
                .unwrap_or_default(),
            InputMode::Direct => self.replay_tail_chunks(),
        }
    }

    fn filter_file_transfer_output(
        &self,
        stream_id: &str,
        chunk: &[u8],
    ) -> (Vec<Vec<u8>>, Vec<FileTransferEvent>) {
        self.file_transfers
            .lock()
            .expect("file transfer lock poisoned")
            .process_output(stream_id, chunk)
    }

    fn broadcast_file_transfer_events(&self, events: Vec<FileTransferEvent>) {
        for event in events {
            match event {
                FileTransferEvent::Download {
                    id,
                    filename_base64,
                    data_base64,
                } => {
                    let _ = self.output_tx.send(SessionEvent::FileDownload {
                        id,
                        filename_base64,
                        data_base64,
                    });
                }
                FileTransferEvent::Status {
                    id,
                    status,
                    message,
                } => {
                    let _ = self.output_tx.send(SessionEvent::FileTransferStatus {
                        id,
                        status,
                        message,
                    });
                }
            }
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
                "list-windows -F 'WT_WINDOW\t{generation}\t#{{window_id}}\t#{{window_index}}\t#{{window_name}}\t#{{window_active}}\t#{{window_zoomed_flag}}'\n\
                 list-panes -s -F 'WT_PANE\t{generation}\t#{{window_id}}\t#{{pane_id}}\t#{{pane_index}}\t#{{pane_active}}\t#{{pane_current_command}}\t#{{pane_current_path}}\t#{{pane_left}}\t#{{pane_top}}\t#{{pane_width}}\t#{{pane_height}}'\n\
                 display-message -p 'WT_DONE\t{generation}'\n"
            )
            .into_bytes(),
        )
    }

    fn request_tmux_pane_captures(&self, panes: Vec<TmuxPane>) -> Result<()> {
        if panes.is_empty() {
            return Ok(());
        }

        let panes = panes
            .into_iter()
            .filter(|pane| should_capture_pane_snapshot(&pane.current_command))
            .map(|pane| pane.id)
            .collect::<Vec<_>>();
        if panes.is_empty() {
            return Ok(());
        }

        if let InputMode::TmuxControl(state) = &self.input_mode {
            let mut state = state.lock().expect("tmux control state lock poisoned");
            state.pending_capture_queue.extend(panes.iter().cloned());
        }

        let mut commands = String::new();
        for pane in panes {
            commands.push_str(&format!(
                "capture-pane -e -p -t {} -S -{}\n",
                tmux_quote(&pane),
                CAPTURE_PANE_HISTORY_LINES
            ));
        }
        self.write_raw_input(commands.into_bytes())
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
            TmuxUiCommand::ZoomPane => {
                format!("resize-pane -Z{}\n", tmux_target_arg(target.as_deref()))
            }
        };
        self.write_raw_input(command.into_bytes())?;
        self.request_tmux_state_refresh()
    }

    fn zoom_tmux_pane(&self, pane_id: &str) -> Result<()> {
        self.write_raw_input(format!("resize-pane -Z -t {}\n", tmux_quote(pane_id)).into_bytes())?;
        self.request_tmux_state_refresh()
    }

    fn resize_tmux_pane(
        &self,
        pane_id: &str,
        direction: TmuxResizeDirection,
        amount: u16,
    ) -> Result<()> {
        let amount = amount.clamp(1, 80);
        let flag = match direction {
            TmuxResizeDirection::Left => "-L",
            TmuxResizeDirection::Right => "-R",
            TmuxResizeDirection::Up => "-U",
            TmuxResizeDirection::Down => "-D",
        };
        self.write_raw_input(
            format!("resize-pane -t {} {flag} {amount}\n", tmux_quote(pane_id)).into_bytes(),
        )?;
        self.request_tmux_state_refresh()
    }

    fn rename_tmux_window(&self, window_id: &str, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            return Ok(());
        }

        self.write_raw_input(
            format!(
                "rename-window -t {} {}\n",
                tmux_quote(window_id),
                tmux_quote(name)
            )
            .into_bytes(),
        )?;
        self.request_tmux_state_refresh()
    }

    fn set_tmux_pane_note(&self, pane_id: &str, note: &str) -> Result<()> {
        let snapshot = match &self.input_mode {
            InputMode::TmuxControl(state) => {
                let mut state = state.lock().expect("tmux control state lock poisoned");
                let pane_exists = state
                    .windows
                    .iter()
                    .any(|window| window.panes.iter().any(|pane| pane.id == pane_id));
                if !pane_exists {
                    state.pane_notes.remove(pane_id);
                    None
                } else {
                    let note = note.trim();
                    if note.is_empty() {
                        state.pane_notes.remove(pane_id);
                    } else {
                        state
                            .pane_notes
                            .insert(pane_id.to_string(), note.to_string());
                    }
                    let pane_note = state.pane_notes.get(pane_id).cloned();
                    for pane in state
                        .windows
                        .iter_mut()
                        .flat_map(|window| window.panes.iter_mut())
                    {
                        if pane.id == pane_id {
                            pane.note = pane_note.clone();
                        }
                    }
                    Some(state.snapshot())
                }
            }
            InputMode::Direct => None,
        };

        if let Some(snapshot) = snapshot {
            let _ = self.output_tx.send(SessionEvent::TmuxState(snapshot));
        }
        Ok(())
    }

    fn start_file_download(&self, id: &str, path: &str, pane_id: Option<String>) -> Result<()> {
        validate_transfer_id(id)?;
        self.file_transfers
            .lock()
            .expect("file transfer lock poisoned")
            .expect_download(id);
        self.write_input(file_download_command(id, path).into_bytes(), pane_id)
    }

    fn start_file_upload(
        &self,
        id: &str,
        path: &str,
        tmp_path: &str,
        pane_id: Option<String>,
    ) -> Result<()> {
        validate_transfer_id(id)?;
        self.write_file_transfer_input(
            file_upload_start_command(path, tmp_path).into_bytes(),
            pane_id,
        )
    }

    fn write_file_upload_chunk(
        &self,
        data: &str,
        tmp_path: &str,
        pane_id: Option<String>,
    ) -> Result<()> {
        self.write_file_transfer_input(
            file_upload_chunk_command(data, tmp_path)?.into_bytes(),
            pane_id,
        )
    }

    fn finish_file_upload(
        &self,
        id: &str,
        path: &str,
        tmp_path: &str,
        pane_id: Option<String>,
    ) -> Result<()> {
        validate_transfer_id(id)?;
        for command in file_upload_finish_commands(path, tmp_path) {
            self.write_file_transfer_input(command.into_bytes(), pane_id.clone())?;
        }
        Ok(())
    }

    fn cancel_file_transfer(&self, id: &str) {
        self.file_transfers
            .lock()
            .expect("file transfer lock poisoned")
            .cancel(id);
    }

    fn pane_notes_snapshot(&self) -> Option<HashMap<String, String>> {
        match &self.input_mode {
            InputMode::TmuxControl(state) => Some(
                state
                    .lock()
                    .expect("tmux control state lock poisoned")
                    .pane_notes
                    .clone(),
            ),
            InputMode::Direct => None,
        }
    }

    fn close(&self) {
        self.alive.store(false, Ordering::Relaxed);
        self.kill_child();

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

    fn reconnect_close(&self) {
        self.alive.store(false, Ordering::Relaxed);
        self.kill_child();
    }

    fn mark_exited(&self) {
        if self.alive.swap(false, Ordering::Relaxed) {
            self.append_direct_output(
                b"\r\n[terminal process exited; reopen the session to reconnect]\r\n",
            );
        }
    }

    fn kill_child(&self) {
        if let Some(mut child) = self.child.lock().expect("child lock poisoned").take()
            && let Err(err) = child.kill()
        {
            warn!(session = %self.id, error = %err, "failed to kill terminal child");
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

    fn replace(&mut self, chunk: Vec<u8>) {
        self.chunks.clear();
        self.bytes = 0;
        self.push(chunk);
    }

    fn tail_chunks(&self, max_bytes: usize) -> Vec<Vec<u8>> {
        let mut selected = Vec::new();
        let mut bytes = 0;

        for chunk in self.chunks.iter().rev() {
            if !selected.is_empty() && bytes + chunk.len() > max_bytes {
                break;
            }
            bytes += chunk.len();
            selected.push(chunk.clone());
            if bytes >= max_bytes {
                break;
            }
        }

        selected.reverse();
        selected
    }
}

#[derive(Default)]
struct FileTransferFilter {
    expected_downloads: HashSet<String>,
    streams: HashMap<String, FileTransferStream>,
}

#[derive(Default)]
struct FileTransferStream {
    pending: Vec<u8>,
    active_download: Option<FileDownloadCapture>,
}

struct FileDownloadCapture {
    id: String,
    filename_base64: String,
    data_base64: String,
}

#[derive(Clone)]
struct UploadState {
    pane_id: Option<String>,
    path: String,
    tmp_path: String,
}

enum FileTransferEvent {
    Download {
        id: String,
        filename_base64: String,
        data_base64: String,
    },
    Status {
        id: String,
        status: String,
        message: String,
    },
}

impl FileTransferFilter {
    fn expect_download(&mut self, id: &str) {
        self.expected_downloads.insert(id.to_string());
    }

    fn cancel(&mut self, id: &str) {
        self.expected_downloads.remove(id);
        for stream in self.streams.values_mut() {
            if stream
                .active_download
                .as_ref()
                .is_some_and(|download| download.id == id)
            {
                stream.active_download = None;
            }
        }
    }

    fn process_output(
        &mut self,
        stream_id: &str,
        chunk: &[u8],
    ) -> (Vec<Vec<u8>>, Vec<FileTransferEvent>) {
        let has_active = self
            .streams
            .get(stream_id)
            .and_then(|stream| stream.active_download.as_ref())
            .is_some();
        if self.expected_downloads.is_empty() && !has_active {
            return (vec![chunk.to_vec()], Vec::new());
        }

        let stream = self.streams.entry(stream_id.to_string()).or_default();
        stream.pending.extend_from_slice(chunk);
        let mut visible = Vec::new();
        let mut events = Vec::new();

        while let Some(pos) = stream.pending.iter().position(|byte| *byte == b'\n') {
            let line = stream.pending.drain(..=pos).collect::<Vec<_>>();
            let line_text = normalized_line_text(&line);
            let marker_text = trim_terminal_marker_prefix(line_text);

            if let Some(download) = stream.active_download.as_mut() {
                if let Some(id) = marker_text.strip_prefix(DOWNLOAD_END_MARKER) {
                    let id = id.trim();
                    if id == download.id {
                        let download = stream.active_download.take().expect("active download");
                        self.expected_downloads.remove(&download.id);
                        events.push(FileTransferEvent::Download {
                            id: download.id,
                            filename_base64: download.filename_base64,
                            data_base64: download.data_base64,
                        });
                        continue;
                    }
                }

                if let Some(rest) = marker_text.strip_prefix(DOWNLOAD_ERROR_MARKER) {
                    let (id, message) = split_transfer_marker(rest);
                    if id == download.id {
                        stream.active_download = None;
                        self.expected_downloads.remove(id);
                        events.push(FileTransferEvent::Status {
                            id: id.to_string(),
                            status: "error".to_string(),
                            message: message.unwrap_or("download failed").to_string(),
                        });
                        continue;
                    }
                }

                download.data_base64.push_str(marker_text.trim());
                if download.data_base64.len() > FILE_TRANSFER_BASE64_LIMIT {
                    let id = download.id.clone();
                    stream.active_download = None;
                    self.expected_downloads.remove(&id);
                    events.push(FileTransferEvent::Status {
                        id,
                        status: "error".to_string(),
                        message: "download is too large for the web terminal bridge".to_string(),
                    });
                }
                continue;
            }

            if let Some(rest) = marker_text.strip_prefix(DOWNLOAD_BEGIN_MARKER) {
                let (id, filename_base64) = split_transfer_marker(rest);
                if self.expected_downloads.contains(id) {
                    stream.active_download = Some(FileDownloadCapture {
                        id: id.to_string(),
                        filename_base64: filename_base64.unwrap_or("ZmlsZQ==").to_string(),
                        data_base64: String::new(),
                    });
                    continue;
                }
            }

            if let Some(rest) = marker_text.strip_prefix(DOWNLOAD_ERROR_MARKER) {
                let (id, message) = split_transfer_marker(rest);
                if self.expected_downloads.remove(id) {
                    events.push(FileTransferEvent::Status {
                        id: id.to_string(),
                        status: "error".to_string(),
                        message: message.unwrap_or("download failed").to_string(),
                    });
                    continue;
                }
            }

            visible.push(line);
        }

        if stream.active_download.is_none()
            && !stream.pending.is_empty()
            && stream.pending.len() > 8192
        {
            visible.push(std::mem::take(&mut stream.pending));
        }

        (visible, events)
    }
}

fn normalized_line_text(line: &[u8]) -> &str {
    let without_lf = line.strip_suffix(b"\n").unwrap_or(line);
    let trimmed = without_lf.strip_suffix(b"\r").unwrap_or(without_lf);
    std::str::from_utf8(trimmed).unwrap_or("")
}

fn trim_terminal_marker_prefix(mut text: &str) -> &str {
    loop {
        if let Some(rest) = text.strip_prefix("\x1bk")
            && let Some(end) = rest.find("\x1b\\")
        {
            text = &rest[end + 2..];
            continue;
        }
        if let Some(rest) = text.strip_prefix("\x1b]")
            && let Some(end) = rest.find('\x07')
        {
            text = &rest[end + 1..];
            continue;
        }
        if let Some(rest) = text.strip_prefix("\x1b]")
            && let Some(end) = rest.find("\x1b\\")
        {
            text = &rest[end + 2..];
            continue;
        }
        if let Some(rest) = text.strip_prefix('\r') {
            text = rest;
            continue;
        }
        break;
    }
    text
}

fn split_transfer_marker(rest: &str) -> (&str, Option<&str>) {
    rest.trim()
        .split_once(':')
        .map_or((rest.trim(), None), |(id, value)| {
            (id.trim(), Some(value.trim()))
        })
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
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
    zoom_pane_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SshTarget {
    host: String,
    username: String,
    port: Option<u16>,
    key_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedSession {
    id: Uuid,
    name: String,
    mode: SessionMode,
    tmux_name: String,
    cols: u16,
    rows: u16,
    ssh: Option<SshTarget>,
    pane_notes: HashMap<String, String>,
    created_at: DateTime<Utc>,
}

#[derive(Default, Deserialize, Serialize)]
struct SessionStoreFile {
    sessions: Vec<PersistedSession>,
}

struct SessionStore {
    path: PathBuf,
    sessions: Mutex<HashMap<Uuid, PersistedSession>>,
}

impl SessionStore {
    fn load(data_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&data_dir)
            .with_context(|| format!("failed to create data dir {}", data_dir.display()))?;
        let path = data_dir.join("sessions.json");
        let file = if path.exists() {
            let raw = fs::read_to_string(&path)
                .with_context(|| format!("failed to read session store {}", path.display()))?;
            serde_json::from_str::<SessionStoreFile>(&raw)
                .with_context(|| format!("failed to parse session store {}", path.display()))?
        } else {
            SessionStoreFile::default()
        };

        Ok(Self {
            path,
            sessions: Mutex::new(
                file.sessions
                    .into_iter()
                    .map(|session| (session.id, session))
                    .collect(),
            ),
        })
    }

    fn list(&self) -> Vec<PersistedSession> {
        self.sessions
            .lock()
            .expect("session store lock poisoned")
            .values()
            .cloned()
            .collect()
    }

    fn upsert(&self, session: PersistedSession) -> Result<()> {
        {
            self.sessions
                .lock()
                .expect("session store lock poisoned")
                .insert(session.id, session);
        }
        self.flush()
    }

    fn remove(&self, id: &Uuid) -> Result<()> {
        {
            self.sessions
                .lock()
                .expect("session store lock poisoned")
                .remove(id);
        }
        self.flush()
    }

    fn update_notes(&self, id: Uuid, pane_notes: HashMap<String, String>) -> Result<()> {
        {
            if let Some(session) = self
                .sessions
                .lock()
                .expect("session store lock poisoned")
                .get_mut(&id)
            {
                session.pane_notes = pane_notes;
            } else {
                return Ok(());
            }
        }
        self.flush()
    }

    fn update_size(&self, id: Uuid, cols: u16, rows: u16) -> Result<()> {
        {
            if let Some(session) = self
                .sessions
                .lock()
                .expect("session store lock poisoned")
                .get_mut(&id)
            {
                session.cols = cols;
                session.rows = rows;
            } else {
                return Ok(());
            }
        }
        self.flush()
    }

    fn flush(&self) -> Result<()> {
        let sessions = {
            let mut sessions = self
                .sessions
                .lock()
                .expect("session store lock poisoned")
                .values()
                .cloned()
                .collect::<Vec<_>>();
            sessions.sort_by_key(|session| session.created_at);
            SessionStoreFile { sessions }
        };
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow!("session store path has no parent"))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create session store dir {}", parent.display()))?;
        let mut temp =
            NamedTempFile::new_in(parent).context("failed to create temporary session store")?;
        serde_json::to_writer_pretty(&mut temp, &sessions)
            .context("failed to write session store")?;
        temp.persist(&self.path)
            .with_context(|| format!("failed to persist session store {}", self.path.display()))?;
        Ok(())
    }
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
    alive: bool,
}

#[derive(Debug, Deserialize)]
struct WsParams {
    readonly: Option<bool>,
    pane_view: Option<bool>,
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

    let store = Arc::new(SessionStore::load(
        std::env::var("WEBTERMINAL_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("../data")),
    )?);
    let sessions = Arc::new(SessionRegistry::new());
    restore_persisted_sessions(&sessions, &store);

    let state = AppState { sessions, store };

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
        .route("/api/sessions/{id}/reconnect", post(reconnect_session))
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

fn restore_persisted_sessions(registry: &Arc<SessionRegistry>, store: &Arc<SessionStore>) {
    for persisted in store.list() {
        match build_persisted_spec(persisted.clone())
            .and_then(|spec| spawn_session(persisted.id, spec, persisted.cols, persisted.rows))
        {
            Ok(session) => {
                info!(session = %persisted.id, name = %persisted.name, "restored persisted session");
                registry.insert(session);
            }
            Err(err) => {
                warn!(session = %persisted.id, name = %persisted.name, error = %err, "failed to restore persisted session");
            }
        }
    }
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

    let spec = build_command(id, &tmux_name, req, cols, rows)?;
    let session = spawn_session(id, spec, cols, rows)?;
    let summary = session.summary();
    if let Some(persistent) = session.persistent.clone() {
        state.store.upsert(persistent)?;
    }
    state.sessions.insert(session);

    Ok(Json(summary))
}

async fn delete_session(State(state): State<AppState>, Path(id): Path<Uuid>) -> StatusCode {
    if let Some(session) = state.sessions.remove(&id) {
        session.close();
        if let Err(err) = state.store.remove(&id) {
            warn!(session = %id, error = %err, "failed to remove persisted session");
        }
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn reconnect_session(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<SessionSummary>, AppError> {
    let old_session = state
        .sessions
        .get(&id)
        .ok_or_else(|| anyhow!("session not found"))?;
    let persistent = old_session
        .persistent
        .clone()
        .ok_or_else(|| anyhow!("session is not reconnectable"))?;

    let session = build_persisted_spec(persistent.clone())
        .and_then(|spec| spawn_session(persistent.id, spec, persistent.cols, persistent.rows))?;
    old_session.reconnect_close();
    let summary = session.summary();
    state.sessions.replace(id, session);

    Ok(Json(summary))
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
    if let Some(pane_id) = req.zoom_pane_id.as_deref() {
        session.zoom_tmux_pane(pane_id)?;
    }
    session.resize(cols, rows)?;
    if let Err(err) = state.store.update_size(id, cols, rows) {
        warn!(session = %id, error = %err, "failed to persist session size");
    }

    Ok(Json(session.summary()))
}

async fn ws_session(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(params): Query<WsParams>,
) -> Result<Response, AppError> {
    let mut session = state
        .sessions
        .get(&id)
        .ok_or_else(|| anyhow!("session not found"))?;
    session = ensure_session_running(&state.sessions, session);
    let readonly = params.readonly.unwrap_or(false);
    let count_viewer = !params.pane_view.unwrap_or(false);

    Ok(ws
        .max_message_size(1024 * 1024)
        .max_frame_size(1024 * 1024)
        .on_upgrade(move |socket| {
            handle_socket(socket, session, state.store, readonly, count_viewer)
        }))
}

fn ensure_session_running(
    registry: &Arc<SessionRegistry>,
    session: Arc<TerminalSession>,
) -> Arc<TerminalSession> {
    if session.is_alive() {
        return session;
    }

    let Some(persisted) = session.persistent.clone() else {
        return session;
    };

    match build_persisted_spec(persisted.clone())
        .and_then(|spec| spawn_session(persisted.id, spec, persisted.cols, persisted.rows))
    {
        Ok(reconnected) => {
            info!(session = %persisted.id, name = %persisted.name, "reconnected persisted session");
            registry.insert(Arc::clone(&reconnected));
            reconnected
        }
        Err(err) => {
            warn!(session = %persisted.id, name = %persisted.name, error = %err, "failed to reconnect persisted session");
            session.append_direct_output(
                format!("\r\n[failed to reconnect persisted session: {err}]\r\n").as_bytes(),
            );
            session
        }
    }
}

async fn handle_socket(
    mut socket: WebSocket,
    session: Arc<TerminalSession>,
    store: Arc<SessionStore>,
    readonly: bool,
    count_viewer: bool,
) {
    if count_viewer {
        session.viewers.fetch_add(1, Ordering::Relaxed);
    }
    let mut rx = session.output_tx.subscribe();
    let mut focused_pane = session.default_pane_id();
    let mut uploads: HashMap<String, UploadState> = HashMap::new();

    if let Some(state) = session.tmux_state_snapshot() {
        let has_tmux_panes = state.windows.iter().any(|window| !window.panes.is_empty());
        if send_server_message(&mut socket, &ServerMessage::TmuxState { state })
            .await
            .is_err()
        {
            if count_viewer {
                session.viewers.fetch_sub(1, Ordering::Relaxed);
            }
            return;
        }
        if has_tmux_panes {
            if let Some(pane_id) = focused_pane.as_deref() {
                if send_focus_and_replay(&mut socket, &session, pane_id)
                    .await
                    .is_err()
                {
                    if count_viewer {
                        session.viewers.fetch_sub(1, Ordering::Relaxed);
                    }
                    return;
                }
            }
        } else {
            for chunk in session.replay_tail_chunks() {
                if socket.send(Message::Binary(chunk.into())).await.is_err() {
                    if count_viewer {
                        session.viewers.fetch_sub(1, Ordering::Relaxed);
                    }
                    return;
                }
            }
        }
        if !has_tmux_panes && let Err(err) = session.request_tmux_state_refresh() {
            warn!(session = %session.id, error = %err, "failed to request tmux state");
        }
    } else {
        for chunk in session.replay_tail_chunks() {
            if socket.send(Message::Binary(chunk.into())).await.is_err() {
                if count_viewer {
                    session.viewers.fetch_sub(1, Ordering::Relaxed);
                }
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
                            Ok(ClientMessage::TmuxCommand { command, pane_id }) => {
                                let target_pane =
                                    pane_id.clone().or_else(|| focused_pane.clone());
                                if pane_id.is_some() {
                                    focused_pane = pane_id;
                                }
                                if let Err(err) =
                                    session.send_tmux_ui_command(command, target_pane.as_deref())
                                {
                                    warn!(session = %session.id, error = %err, "failed to send tmux UI command");
                                    break;
                                }
                            }
                            Ok(ClientMessage::Paste { data, pane_id }) => {
                                let target_pane =
                                    pane_id.clone().or_else(|| focused_pane.clone());
                                if pane_id.is_some() {
                                    focused_pane = pane_id;
                                }
                                if let Err(err) =
                                    session.write_paste(data.into_bytes(), target_pane)
                                {
                                    warn!(session = %session.id, error = %err, "failed to write paste input");
                                    break;
                                }
                            }
                            Ok(ClientMessage::ResizePane { pane_id, direction, amount }) => {
                                focused_pane = Some(pane_id.clone());
                                if let Err(err) =
                                    session.resize_tmux_pane(&pane_id, direction, amount)
                                {
                                    warn!(session = %session.id, error = %err, "failed to resize tmux pane");
                                    break;
                                }
                            }
                            Ok(ClientMessage::RenameWindow { window_id, name }) => {
                                if let Err(err) = session.rename_tmux_window(&window_id, &name) {
                                    warn!(session = %session.id, error = %err, "failed to rename tmux window");
                                    break;
                                }
                            }
                            Ok(ClientMessage::SetPaneNote { pane_id, note }) => {
                                if let Err(err) = session.set_tmux_pane_note(&pane_id, &note) {
                                    warn!(session = %session.id, error = %err, "failed to set tmux pane note");
                                    break;
                                }
                                if let Some(pane_notes) = session.pane_notes_snapshot()
                                    && let Err(err) =
                                        store.update_notes(session.id, pane_notes)
                                {
                                    warn!(session = %session.id, error = %err, "failed to persist pane notes");
                                }
                            }
                            Ok(ClientMessage::FileDownload { id, path, pane_id }) => {
                                let target_pane =
                                    pane_id.clone().or_else(|| focused_pane.clone());
                                if pane_id.is_some() {
                                    focused_pane = pane_id;
                                }
                                if let Err(err) =
                                    session.start_file_download(&id, &path, target_pane)
                                {
                                    warn!(session = %session.id, error = %err, "failed to start file download");
                                    let _ = send_server_message(
                                        &mut socket,
                                        &ServerMessage::FileTransferStatus {
                                            id: &id,
                                            status: "error",
                                            message: &err.to_string(),
                                        },
                                    )
                                    .await;
                                }
                            }
                            Ok(ClientMessage::FileUploadStart { id, path, pane_id }) => {
                                let target_pane =
                                    pane_id.clone().or_else(|| focused_pane.clone());
                                if pane_id.is_some() {
                                    focused_pane = pane_id;
                                }
                                let tmp_path = upload_tmp_path(&path, &id);
                                match session.start_file_upload(&id, &path, &tmp_path, target_pane.clone()) {
                                    Ok(()) => {
                                        uploads.insert(
                                            id,
                                            UploadState {
                                                pane_id: target_pane,
                                                path,
                                                tmp_path,
                                            },
                                        );
                                    }
                                    Err(err) => {
                                        warn!(session = %session.id, error = %err, "failed to start file upload");
                                        let _ = send_server_message(
                                            &mut socket,
                                            &ServerMessage::FileTransferStatus {
                                                id: &id,
                                                status: "error",
                                                message: &err.to_string(),
                                            },
                                        )
                                        .await;
                                    }
                                }
                            }
                            Ok(ClientMessage::FileUploadChunk { id, data }) => {
                                if let Some(target_pane) = uploads.get(&id).cloned()
                                    && let Err(err) =
                                        session.write_file_upload_chunk(&data, &target_pane.tmp_path, target_pane.pane_id)
                                {
                                    warn!(session = %session.id, error = %err, "failed to write file upload chunk");
                                    uploads.remove(&id);
                                    let _ = send_server_message(
                                        &mut socket,
                                        &ServerMessage::FileTransferStatus {
                                            id: &id,
                                            status: "error",
                                            message: &err.to_string(),
                                        },
                                    )
                                    .await;
                                }
                            }
                            Ok(ClientMessage::FileUploadFinish { id }) => {
                                if let Some(upload) = uploads.remove(&id)
                                    && let Err(err) = session.finish_file_upload(
                                        &id,
                                        &upload.path,
                                        &upload.tmp_path,
                                        upload.pane_id,
                                    )
                                {
                                    warn!(session = %session.id, error = %err, "failed to finish file upload");
                                    let _ = send_server_message(
                                        &mut socket,
                                        &ServerMessage::FileTransferStatus {
                                            id: &id,
                                            status: "error",
                                            message: &err.to_string(),
                                        },
                                    )
                                    .await;
                                }
                            }
                            Ok(ClientMessage::FileTransferCancel { id }) => {
                                uploads.remove(&id);
                                session.cancel_file_transfer(&id);
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
                    Ok(SessionEvent::FileDownload { id, filename_base64, data_base64 }) => {
                        if send_server_message(
                            &mut socket,
                            &ServerMessage::FileDownload {
                                id: &id,
                                filename_base64: &filename_base64,
                                data_base64: &data_base64,
                            },
                        )
                        .await
                        .is_err()
                        {
                            break;
                        }
                    }
                    Ok(SessionEvent::FileTransferStatus { id, status, message }) => {
                        if send_server_message(
                            &mut socket,
                            &ServerMessage::FileTransferStatus {
                                id: &id,
                                status: &status,
                                message: &message,
                            },
                        )
                        .await
                        .is_err()
                        {
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

    if count_viewer {
        session.viewers.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn send_focus_and_replay(
    socket: &mut WebSocket,
    session: &TerminalSession,
    pane_id: &str,
) -> Result<()> {
    send_server_message(socket, &ServerMessage::FocusPane { pane_id }).await?;
    send_server_message(socket, &ServerMessage::Clear).await?;
    for chunk in session.replay_pane_tail_chunks(pane_id) {
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
        persistent: spec.persistent,
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
        file_transfers: Mutex::new(FileTransferFilter::default()),
        viewers: AtomicUsize::new(0),
        alive: std::sync::atomic::AtomicBool::new(true),
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
            let slow_transfer = matches!(input, PtyInput::FileTransfer { .. });
            let payload = match input {
                PtyInput::User { data, pane_id } => {
                    encode_input_for_mode(&input_mode, &data, pane_id.as_deref())
                }
                PtyInput::FileTransfer { data, pane_id } => {
                    encode_input_for_mode(&input_mode, &data, pane_id.as_deref())
                }
                PtyInput::Paste { data, pane_id } => {
                    encode_paste_for_mode(&input_mode, &data, pane_id.as_deref())
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
            if slow_transfer {
                thread::sleep(std::time::Duration::from_millis(300));
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
    session.mark_exited();
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
    session.mark_exited();
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
        if append_tmux_capture_line(state, line) {
            return;
        }
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

    if handle_tmux_capture_control_line(session, state, line) {
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

fn append_tmux_capture_line(state: &Arc<Mutex<TmuxControlState>>, line: &[u8]) -> bool {
    let mut state = state.lock().expect("tmux control state lock poisoned");
    let Some(capture) = state.active_capture.as_mut() else {
        return false;
    };
    let mut line = line.to_vec();
    line.extend_from_slice(b"\r\n");
    capture.lines.push(line);
    true
}

fn handle_tmux_capture_control_line(
    session: &Arc<TerminalSession>,
    state: &Arc<Mutex<TmuxControlState>>,
    line: &[u8],
) -> bool {
    if {
        let state = state.lock().expect("tmux control state lock poisoned");
        state.active_capture.is_some()
    } && !line.starts_with(b"%end ")
        && !line.starts_with(b"%error ")
    {
        append_tmux_capture_line(state, line);
        return true;
    }

    if line.starts_with(b"%begin ") {
        let mut state = state.lock().expect("tmux control state lock poisoned");
        if state.active_capture.is_none()
            && let Some(pane_id) = state.pending_capture_queue.pop_front()
        {
            state.active_capture = Some(TmuxCapture {
                pane_id,
                lines: Vec::new(),
            });
            return true;
        }
        return false;
    }

    if line.starts_with(b"%end ") || line.starts_with(b"%error ") {
        let capture = {
            let mut state = state.lock().expect("tmux control state lock poisoned");
            state.active_capture.take()
        };
        if let Some(capture) = capture {
            if line.starts_with(b"%end ") {
                let snapshot = capture.lines.concat();
                session.replace_pane_snapshot(&capture.pane_id, snapshot);
            }
            return true;
        }
    }

    false
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
        let parts = rest.splitn(6, '\t').collect::<Vec<_>>();
        if parts.len() == 6 {
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
                        zoomed: parts[5] == "1",
                        panes: Vec::new(),
                    },
                );
            }
        }
        return true;
    }

    if let Some(rest) = text.strip_prefix("WT_PANE\t") {
        let parts = rest.splitn(11, '\t').collect::<Vec<_>>();
        if parts.len() == 11 {
            let generation = parts[0].parse::<u64>().ok();
            let mut state = state.lock().expect("tmux control state lock poisoned");
            if state.pending_generation == generation {
                let pane_id = parts[2].to_string();
                let pane = TmuxPane {
                    window_id: parts[1].to_string(),
                    id: pane_id.clone(),
                    index: parts[3].parse::<u16>().ok(),
                    active: parts[4] == "1",
                    current_command: parts[5].to_string(),
                    current_path: parts[6].to_string(),
                    note: state.pane_notes.get(&pane_id).cloned(),
                    left: parts[7].parse::<u16>().ok(),
                    top: parts[8].parse::<u16>().ok(),
                    width: parts[9].parse::<u16>().ok(),
                    height: parts[10].parse::<u16>().ok(),
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
        let (snapshot, panes_to_capture) = {
            let mut state = state.lock().expect("tmux control state lock poisoned");
            if state.pending_generation == generation {
                let mut windows = std::mem::take(&mut state.pending_windows);
                let mut live_panes = HashSet::new();
                for pane in std::mem::take(&mut state.pending_panes).into_values() {
                    live_panes.insert(pane.id.clone());
                    windows
                        .entry(pane.window_id.clone())
                        .or_insert_with(|| TmuxWindow {
                            id: pane.window_id.clone(),
                            index: None,
                            name: pane.window_id.clone(),
                            active: false,
                            zoomed: false,
                            panes: Vec::new(),
                        })
                        .panes
                        .push(pane);
                }
                state.windows = windows.into_values().collect();
                state
                    .pane_notes
                    .retain(|pane_id, _| live_panes.contains(pane_id));
                state
                    .pane_replay
                    .retain(|pane_id, _| live_panes.contains(pane_id));
                if !state
                    .active_pane
                    .as_ref()
                    .is_some_and(|pane_id| live_panes.contains(pane_id))
                {
                    state.active_pane = state
                        .windows
                        .iter()
                        .flat_map(|window| window.panes.iter())
                        .find(|pane| pane.active)
                        .or_else(|| {
                            state
                                .windows
                                .iter()
                                .flat_map(|window| window.panes.iter())
                                .next()
                        })
                        .map(|pane| pane.id.clone());
                }
                for window in &mut state.windows {
                    window
                        .panes
                        .sort_by_key(|pane| pane.index.unwrap_or(u16::MAX));
                }
                state
                    .windows
                    .sort_by_key(|window| window.index.unwrap_or(u16::MAX));
                state.pending_generation = None;
                let panes_to_capture = state
                    .windows
                    .iter()
                    .flat_map(|window| window.panes.iter())
                    .cloned()
                    .collect::<Vec<_>>();
                (Some(state.snapshot()), panes_to_capture)
            } else {
                (None, Vec::new())
            }
        };
        if let Some(snapshot) = snapshot {
            let _ = session.output_tx.send(SessionEvent::TmuxState(snapshot));
            if let Err(err) = session.request_tmux_pane_captures(panes_to_capture) {
                warn!(session = %session.id, error = %err, "failed to capture tmux panes");
            }
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

fn should_capture_pane_snapshot(command: &str) -> bool {
    let command = command.rsplit('/').next().unwrap_or(command);
    !matches!(
        command,
        "vi" | "vim"
            | "nvim"
            | "view"
            | "less"
            | "more"
            | "man"
            | "top"
            | "htop"
            | "btop"
            | "nano"
            | "emacs"
            | "scp"
            | "sftp"
    )
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
            if let Some(pane) = tmux_target_pane(state, pane_override) {
                encode_tmux_control_input(&pane, data)
            } else {
                data.to_vec()
            }
        }
    }
}

fn encode_paste_for_mode(mode: &InputMode, data: &[u8], pane_override: Option<&str>) -> Vec<u8> {
    match mode {
        InputMode::Direct => data.to_vec(),
        InputMode::TmuxControl(state) => {
            if let Some(pane) = tmux_target_pane(state, pane_override) {
                encode_tmux_control_paste(&pane, data)
            } else {
                data.to_vec()
            }
        }
    }
}

fn tmux_target_pane(
    state: &Arc<Mutex<TmuxControlState>>,
    pane_override: Option<&str>,
) -> Option<String> {
    let state = state.lock().expect("tmux control state lock poisoned");
    if !state.initialized {
        return None;
    }
    pane_override
        .map(ToOwned::to_owned)
        .or_else(|| state.active_pane.clone())
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

fn encode_tmux_control_paste(pane: &str, data: &[u8]) -> Vec<u8> {
    let mut commands = String::new();
    commands.push_str(&format!("send-keys -t {pane} Escape\n"));
    commands.push_str(&format!("send-keys -t {pane} -l -- '[200~'\n"));

    let text = String::from_utf8_lossy(data);
    let mut literal = String::new();
    for c in text.chars() {
        match c {
            '\r' => {}
            '\n' => {
                flush_tmux_literal(&mut commands, pane, &mut literal);
                commands.push_str(&format!("send-keys -t {pane} Enter\n"));
            }
            '\t' => {
                flush_tmux_literal(&mut commands, pane, &mut literal);
                commands.push_str(&format!("send-keys -t {pane} Tab\n"));
            }
            c => {
                literal.push(c);
                if literal.len() >= 2048 {
                    flush_tmux_literal(&mut commands, pane, &mut literal);
                }
            }
        }
    }

    flush_tmux_literal(&mut commands, pane, &mut literal);
    commands.push_str(&format!("send-keys -t {pane} Escape\n"));
    commands.push_str(&format!("send-keys -t {pane} -l -- '[201~'\n"));
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
    persistent: Option<PersistedSession>,
}

fn build_command(
    id: Uuid,
    tmux_name: &str,
    req: CreateSessionRequest,
    cols: u16,
    rows: u16,
) -> Result<SessionSpec> {
    if let Some(command) = req.command {
        let name = req.name.unwrap_or_else(|| "Custom Command".to_string());
        return Ok(SessionSpec {
            name,
            command,
            cleanup_command: None,
            mode: SessionMode::Local,
            input_mode: InputMode::Direct,
            persistent: None,
        });
    }

    match req.mode.unwrap_or(CreateMode::LocalCc) {
        CreateMode::Local => {
            let name = req
                .name
                .unwrap_or_else(|| format!("Local {short}", short = short_id(id)));
            let command = format!("tmux new-session -A -s {}", shell_escape(tmux_name));
            let cleanup_command = Some(format!("tmux kill-session -t {}", shell_escape(tmux_name)));
            let persistent = persisted_session(
                id,
                name.clone(),
                SessionMode::Local,
                tmux_name,
                cols,
                rows,
                None,
            );
            Ok(SessionSpec {
                name,
                command,
                cleanup_command,
                mode: SessionMode::Local,
                input_mode: InputMode::Direct,
                persistent: Some(persistent),
            })
        }
        CreateMode::LocalCc => {
            let name = req
                .name
                .unwrap_or_else(|| format!("Local CC {short}", short = short_id(id)));
            let command = format!("tmux -CC new-session -A -s {}", shell_escape(tmux_name));
            let cleanup_command = Some(format!("tmux kill-session -t {}", shell_escape(tmux_name)));
            let persistent = persisted_session(
                id,
                name.clone(),
                SessionMode::LocalCc,
                tmux_name,
                cols,
                rows,
                None,
            );
            Ok(SessionSpec {
                name,
                command,
                cleanup_command,
                mode: SessionMode::LocalCc,
                input_mode: InputMode::TmuxControl(Arc::new(Mutex::new(
                    TmuxControlState::default(),
                ))),
                persistent: Some(persistent),
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
            let persistent = persisted_session(
                id,
                name.clone(),
                SessionMode::Ssh,
                tmux_name,
                cols,
                rows,
                Some(ssh.clone()),
            );
            Ok(SessionSpec {
                name,
                command: parts.join(" "),
                cleanup_command: None,
                mode: SessionMode::Ssh,
                input_mode: InputMode::Direct,
                persistent: Some(persistent),
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
            let persistent = persisted_session(
                id,
                name.clone(),
                SessionMode::SshCc,
                tmux_name,
                cols,
                rows,
                Some(ssh.clone()),
            );
            Ok(SessionSpec {
                name,
                command: parts.join(" "),
                cleanup_command: None,
                mode: SessionMode::SshCc,
                input_mode: InputMode::TmuxControl(Arc::new(Mutex::new(
                    TmuxControlState::default(),
                ))),
                persistent: Some(persistent),
            })
        }
    }
}

fn persisted_session(
    id: Uuid,
    name: String,
    mode: SessionMode,
    tmux_name: &str,
    cols: u16,
    rows: u16,
    ssh: Option<SshTarget>,
) -> PersistedSession {
    PersistedSession {
        id,
        name,
        mode,
        tmux_name: tmux_name.to_string(),
        cols,
        rows,
        ssh,
        pane_notes: HashMap::new(),
        created_at: Utc::now(),
    }
}

fn build_persisted_spec(session: PersistedSession) -> Result<SessionSpec> {
    let input_mode = persisted_input_mode(&session);
    let command = match session.mode {
        SessionMode::Local => format!(
            "tmux new-session -A -s {}",
            shell_escape(&session.tmux_name)
        ),
        SessionMode::LocalCc => format!(
            "tmux -CC new-session -A -s {}",
            shell_escape(&session.tmux_name)
        ),
        SessionMode::Ssh => {
            let ssh = session
                .ssh
                .as_ref()
                .ok_or_else(|| anyhow!("persisted ssh session is missing ssh target"))?;
            let port = ssh.port.unwrap_or(22);
            ssh_command_parts(
                ssh,
                port,
                &format!(
                    "tmux new-session -A -s {}",
                    shell_escape(&session.tmux_name)
                ),
            )
            .join(" ")
        }
        SessionMode::SshCc => {
            let ssh = session
                .ssh
                .as_ref()
                .ok_or_else(|| anyhow!("persisted ssh control session is missing ssh target"))?;
            let port = ssh.port.unwrap_or(22);
            ssh_command_parts(
                ssh,
                port,
                &format!(
                    "tmux -CC new-session -A -s {}",
                    shell_escape(&session.tmux_name)
                ),
            )
            .join(" ")
        }
    };

    let cleanup_command = match session.mode {
        SessionMode::Local | SessionMode::LocalCc => Some(format!(
            "tmux kill-session -t {}",
            shell_escape(&session.tmux_name)
        )),
        SessionMode::Ssh | SessionMode::SshCc => None,
    };

    Ok(SessionSpec {
        name: session.name.clone(),
        command,
        cleanup_command,
        mode: session.mode,
        input_mode,
        persistent: Some(session),
    })
}

fn persisted_input_mode(session: &PersistedSession) -> InputMode {
    match session.mode {
        SessionMode::LocalCc | SessionMode::SshCc => {
            let control = TmuxControlState {
                pane_notes: session.pane_notes.clone(),
                ..TmuxControlState::default()
            };
            InputMode::TmuxControl(Arc::new(Mutex::new(control)))
        }
        SessionMode::Local | SessionMode::Ssh => InputMode::Direct,
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

fn validate_transfer_id(id: &str) -> Result<()> {
    if !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    {
        Ok(())
    } else {
        Err(anyhow!("invalid file transfer id"))
    }
}

fn file_download_command(id: &str, path: &str) -> String {
    let path = shell_escape(path);
    let id = shell_escape(id);
    format!(
        "__wt_id={id}; __wt_path={path}; \
         if [ -f \"$__wt_path\" ] && [ -r \"$__wt_path\" ]; then \
           __wt_name=$(basename -- \"$__wt_path\" 2>/dev/null || basename \"$__wt_path\"); \
           echo \"{DOWNLOAD_BEGIN_MARKER}$__wt_id:$(printf '%s' \"$__wt_name\" | base64 | tr -d '\\n')\"; \
           if base64 < \"$__wt_path\"; then \
             echo \"{DOWNLOAD_END_MARKER}$__wt_id\"; \
           else \
             echo \"{DOWNLOAD_ERROR_MARKER}$__wt_id:base64 failed\"; \
           fi; \
         else \
           echo \"{DOWNLOAD_ERROR_MARKER}$__wt_id:file not readable\"; \
         fi; \
         unset __wt_id __wt_path __wt_name\r"
    )
}

fn upload_tmp_path(path: &str, id: &str) -> String {
    format!("{path}.webterminal.{id}.tmp")
}

fn file_upload_start_command(path: &str, tmp_path: &str) -> String {
    let path = shell_escape(path);
    let tmp_b64 = shell_escape(&format!("{tmp_path}.b64"));
    format!("__wt_path={path}; : > {tmp_b64}\r")
}

fn file_upload_chunk_command(data: &str, tmp_path: &str) -> Result<String> {
    let compact = data
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect::<String>();
    if compact.is_empty() {
        return Ok(String::new());
    }
    if !compact
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
    {
        return Err(anyhow!("file upload chunk is not valid base64 text"));
    }
    let tmp_b64 = shell_escape(&format!("{tmp_path}.b64"));
    Ok(format!(
        "printf '%s' {} >> {tmp_b64}\r",
        shell_escape(&compact),
    ))
}

fn file_upload_finish_commands(path: &str, tmp_path: &str) -> Vec<String> {
    let path = shell_escape(path);
    let tmp_b64 = shell_escape(&format!("{tmp_path}.b64"));
    let tmp_ok = shell_escape(&format!("{tmp_path}.ok"));
    let tmp_path = shell_escape(tmp_path);
    vec![
        format!("rm -f {tmp_ok}\r"),
        format!(
            "(base64 -d < {tmp_b64} > {tmp_path} 2>/dev/null || base64 -D < {tmp_b64} > {tmp_path} 2>/dev/null) && touch {tmp_ok}\r"
        ),
        format!(
            "if [ -f {tmp_ok} ]; then mv {tmp_path} {path} && rm -f {tmp_b64} {tmp_ok} && printf '\\n[webterminal upload complete: %s]\\n' {path}; else rm -f {tmp_path} {tmp_b64} {tmp_ok}; printf '\\n[webterminal upload failed: base64 decode failed]\\n'; fi\r"
        ),
    ]
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
