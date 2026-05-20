import React, { useCallback, useEffect, useMemo, useState } from "react";
import { createRoot } from "react-dom/client";
import {
  ChevronDown,
  ChevronRight,
  ChevronUp,
  Circle,
  Command,
  Copy,
  Download,
  Maximize2,
  Menu,
  Monitor,
  PanelLeftClose,
  PanelLeftOpen,
  Pencil,
  Plus,
  RefreshCw,
  Server,
  Smartphone,
  Trash2,
  Upload,
  Wifi,
  X,
} from "lucide-react";
import { Terminal } from "@xterm/xterm";
import { WebLinksAddon } from "@xterm/addon-web-links";
import "@xterm/xterm/css/xterm.css";
import "./styles.css";

type SessionMode = "local" | "ssh" | "local_cc" | "ssh_cc";
type InteractionMode = "input" | "browse";
type TmuxCommand = "new_window" | "split_horizontal" | "split_vertical" | "kill_pane" | "zoom_pane";
type ResizeDirection = "left" | "right" | "up" | "down";

const ACTIVE_SESSION_STORAGE_KEY = "webterminal.activeSession";
const SIDEBAR_COLLAPSED_STORAGE_KEY = "webterminal.sidebarCollapsed";
const MAX_UPLOAD_BYTES = 24 * 1024 * 1024;
const FILE_UPLOAD_CHUNK_CHARS = 2048;
const DESKTOP_PANE_EDGE_GUARD_PX = 24;
const DESKTOP_PANE_SEPARATOR_GAP_PX = 24;
const TERMINAL_FIT_GUARD_PX = 18;
const TERMINAL_SCROLLBAR_GUARD_PX = 12;

function focusedPaneStorageKey(sessionId: string): string {
  return `webterminal.focusedPane.${sessionId}`;
}

function readStoredValue(key: string): string | null {
  try {
    return window.localStorage.getItem(key);
  } catch {
    return null;
  }
}

function writeStoredValue(key: string, value: string | null) {
  try {
    if (value) {
      window.localStorage.setItem(key, value);
    } else {
      window.localStorage.removeItem(key);
    }
  } catch {
    // Ignore private-mode or quota failures; remembering focus is a convenience.
  }
}

function scrollTerminalToBottomSoon(term: Terminal) {
  if (!term.element?.isConnected) return;
  try {
    term.scrollToBottom();
  } catch {
    return;
  }
  window.requestAnimationFrame(() => {
    if (!term.element?.isConnected) return;
    try {
      term.scrollToBottom();
    } catch {
      // xterm can briefly lack render dimensions while its element is being moved between panes.
    }
  });
}

function terminalIsNearBottom(term: Terminal): boolean {
  const buffer = term.buffer.active;
  return buffer.baseY - buffer.viewportY <= 2;
}

function writeTerminalData(
  term: Terminal,
  data: Uint8Array,
  shouldFollow: boolean,
) {
  const follow = shouldFollow && terminalIsNearBottom(term);
  term.write(data);
  if (follow) {
    scrollTerminalToBottomSoon(term);
  }
}

function attachTerminalKeyHandlers(term: Terminal, sendData: (data: string) => void) {
  term.attachCustomKeyEventHandler((event) => {
    if (event.type === "keydown" && event.key === "Enter" && event.shiftKey) {
      sendData("\n");
      return false;
    }
    return true;
  });
}

function attachPasteHandler(container: HTMLElement, sendPaste: (data: string) => void) {
  const onPaste = (event: ClipboardEvent) => {
    const text = event.clipboardData?.getData("text/plain") ?? "";
    if (!text) return;
    event.preventDefault();
    event.stopPropagation();
    sendPaste(text);
  };

  container.addEventListener("paste", onPaste, { capture: true });
  return () => container.removeEventListener("paste", onPaste, { capture: true });
}

function makeTransferId(): string {
  return window.crypto?.randomUUID?.() ?? `wt-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

function decodeBase64Text(value: string): string {
  const bytes = Uint8Array.from(window.atob(value), (char) => char.charCodeAt(0));
  return new TextDecoder().decode(bytes);
}

function base64ToBlob(value: string): Blob {
  const parts: BlobPart[] = [];
  for (let offset = 0; offset < value.length; offset += 32768) {
    const slice = value.slice(offset, offset + 32768);
    const bytes = Uint8Array.from(window.atob(slice), (char) => char.charCodeAt(0));
    parts.push(bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength));
  }
  return new Blob(parts, { type: "application/octet-stream" });
}

function base64ToBytes(value: string): Uint8Array {
  return Uint8Array.from(window.atob(value), (char) => char.charCodeAt(0));
}

function createTerminal(cols: number, rows: number, fontSize: number): Terminal {
  const term = new Terminal({
    cols,
    rows,
    cursorBlink: true,
    convertEol: false,
    fontFamily:
      "SFMono-Regular, ui-monospace, Menlo, Monaco, Consolas, Liberation Mono, monospace",
    fontSize,
    lineHeight: 1.15,
    scrollback: 50000,
    theme: {
      background: "#101315",
      foreground: "#d7e0e5",
      cursor: "#f3c969",
      selectionBackground: "#31515f",
      black: "#0f1316",
      red: "#d96c68",
      green: "#89b482",
      yellow: "#d9b56c",
      blue: "#6c96d9",
      magenta: "#b482c7",
      cyan: "#6cb7b8",
      white: "#d7e0e5",
      brightBlack: "#5d6a70",
      brightRed: "#f0837d",
      brightGreen: "#9ccc91",
      brightYellow: "#edc879",
      brightBlue: "#81a9ef",
      brightMagenta: "#c798da",
      brightCyan: "#80d0d0",
      brightWhite: "#f1f5f7",
    },
  });
  term.loadAddon(new WebLinksAddon());
  return term;
}

function triggerBrowserDownload(filename: string, dataBase64: string) {
  const blob = base64ToBlob(dataBase64);
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = filename || "download";
  document.body.appendChild(link);
  link.click();
  link.remove();
  window.setTimeout(() => URL.revokeObjectURL(url), 30_000);
}

function readFileAsBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(reader.error ?? new Error("Failed to read file"));
    reader.onload = () => {
      const result = String(reader.result ?? "");
      const comma = result.indexOf(",");
      resolve(comma >= 0 ? result.slice(comma + 1) : result);
    };
    reader.readAsDataURL(file);
  });
}

function chooseLocalFile(): Promise<File | null> {
  return new Promise((resolve) => {
    const input = document.createElement("input");
    input.type = "file";
    input.style.display = "none";
    input.onchange = () => {
      resolve(input.files?.[0] ?? null);
      input.remove();
    };
    document.body.appendChild(input);
    input.click();
  });
}

function waitForSocketBuffer(ws: WebSocket): Promise<void> {
  if (ws.bufferedAmount < 512 * 1024) return Promise.resolve();
  return new Promise((resolve) => {
    const tick = () => {
      if (ws.readyState !== WebSocket.OPEN || ws.bufferedAmount < 512 * 1024) {
        resolve();
      } else {
        window.setTimeout(tick, 25);
      }
    };
    tick();
  });
}

interface SessionSummary {
  id: string;
  name: string;
  command: string;
  mode: SessionMode;
  cols: number;
  rows: number;
  created_at: string;
  viewers: number;
  alive: boolean;
}

interface CreateSessionPayload {
  name?: string;
  mode: SessionMode;
  tmux_name?: string;
  cols: number;
  rows: number;
  ssh?: {
    host: string;
    username: string;
    port?: number;
    key_path?: string;
  };
}

interface ResizeSessionPayload {
  cols: number;
  rows: number;
  zoom_pane_id?: string | null;
}

interface TmuxState {
  active_pane: string | null;
  windows: TmuxWindow[];
}

interface TmuxWindow {
  id: string;
  index: number | null;
  name: string;
  active: boolean;
  zoomed: boolean;
  panes: TmuxPane[];
}

interface TmuxPane {
  id: string;
  window_id: string;
  index: number | null;
  active: boolean;
  current_command: string;
  current_path: string;
  note?: string | null;
  left?: number | null;
  top?: number | null;
  width?: number | null;
  height?: number | null;
}

type ServerMessage =
  | { type: "clear" }
  | { type: "focus_pane"; pane_id: string }
  | { type: "pane_output"; pane_id: string; data_base64: string }
  | { type: "pane_snapshot"; pane_id: string; data_base64: string }
  | { type: "tmux_state"; state: TmuxState }
  | {
      type: "file_download";
      id: string;
      filename_base64: string;
      data_base64: string;
    }
  | {
      type: "file_transfer_status";
      id: string;
      status: "ok" | "error" | string;
      message: string;
    };

interface CachedPaneTerminal {
  term: Terminal;
  element: HTMLDivElement;
  opened: boolean;
  dataSub: { dispose: () => void };
  detachPasteHandler: () => void;
}

const api = {
  async listSessions(): Promise<SessionSummary[]> {
    const res = await fetch("/api/sessions");
    if (!res.ok) throw new Error("Failed to load sessions");
    return res.json();
  },

  async createSession(payload: CreateSessionPayload): Promise<SessionSummary> {
    const res = await fetch("/api/sessions", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
    });
    if (!res.ok) {
      const body = await res.json().catch(() => null);
      throw new Error(body?.error ?? "Failed to create session");
    }
    return res.json();
  },

  async deleteSession(id: string): Promise<void> {
    const res = await fetch(`/api/sessions/${id}`, { method: "DELETE" });
    if (!res.ok && res.status !== 404) throw new Error("Failed to delete session");
  },

  async reconnectSession(id: string): Promise<SessionSummary> {
    const res = await fetch(`/api/sessions/${id}/reconnect`, { method: "POST" });
    if (!res.ok) {
      const body = await res.json().catch(() => null);
      throw new Error(body?.error ?? "Failed to reconnect session");
    }
    return res.json();
  },

  async resizeSession(id: string, payload: ResizeSessionPayload): Promise<SessionSummary> {
    const res = await fetch(`/api/sessions/${id}/resize`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
    });
    if (!res.ok) {
      const body = await res.json().catch(() => null);
      throw new Error(body?.error ?? "Failed to resize session");
    }
    return res.json();
  },
};

function sessionModeLabel(mode: SessionMode): string {
  switch (mode) {
    case "local_cc":
      return "tmux -CC";
    case "ssh_cc":
      return "SSH -CC";
    case "local":
      return "tmux legacy";
    case "ssh":
      return "SSH legacy";
  }
}

function App() {
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [activeId, setActiveId] = useState<string | null>(() =>
    readStoredValue(ACTIVE_SESSION_STORAGE_KEY),
  );
  const [formOpen, setFormOpen] = useState(false);
  const [drawerOpen, setDrawerOpen] = useState(false);
  const [sidebarCollapsed, setSidebarCollapsed] = useState(
    () => readStoredValue(SIDEBAR_COLLAPSED_STORAGE_KEY) === "true",
  );
  const [sessionActionsOpen, setSessionActionsOpen] = useState(false);
  const [sessionMenuId, setSessionMenuId] = useState<string | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<SessionSummary | null>(null);
  const [resizeToken, setResizeToken] = useState(0);
  const [error, setError] = useState<string | null>(null);

  const active = useMemo(
    () => sessions.find((session) => session.id === activeId) ?? null,
    [sessions, activeId],
  );

  const refresh = async () => {
    const next = await api.listSessions();
    setSessions(next);
    setActiveId((current) => {
      if (current && next.some((session) => session.id === current)) return current;
      const stored = readStoredValue(ACTIVE_SESSION_STORAGE_KEY);
      if (stored && next.some((session) => session.id === stored)) return stored;
      return next[0]?.id ?? null;
    });
  };

  useEffect(() => {
    void refresh().catch((err: unknown) => setError(String(err)));
  }, []);

  useEffect(() => {
    const timer = window.setInterval(() => {
      void refresh().catch((err: unknown) => setError(String(err)));
    }, 5000);
    return () => window.clearInterval(timer);
  }, []);

  useEffect(() => {
    const viewport = window.visualViewport;
    const updateAppHeight = () => {
      const height = viewport?.height ?? window.innerHeight;
      document.documentElement.style.setProperty("--app-height", `${height}px`);
    };

    updateAppHeight();
    viewport?.addEventListener("resize", updateAppHeight);
    viewport?.addEventListener("scroll", updateAppHeight);
    window.addEventListener("resize", updateAppHeight);
    window.addEventListener("orientationchange", updateAppHeight);

    return () => {
      viewport?.removeEventListener("resize", updateAppHeight);
      viewport?.removeEventListener("scroll", updateAppHeight);
      window.removeEventListener("resize", updateAppHeight);
      window.removeEventListener("orientationchange", updateAppHeight);
      document.documentElement.style.removeProperty("--app-height");
    };
  }, []);

  useEffect(() => {
    setSessionActionsOpen(false);
    setSessionMenuId(null);
  }, [activeId]);

  useEffect(() => {
    writeStoredValue(ACTIVE_SESSION_STORAGE_KEY, activeId);
  }, [activeId]);

  useEffect(() => {
    writeStoredValue(SIDEBAR_COLLAPSED_STORAGE_KEY, sidebarCollapsed ? "true" : null);
  }, [sidebarCollapsed]);

  const createSession = async (payload: CreateSessionPayload) => {
    setError(null);
    const session = await api.createSession(payload);
    await refresh();
    setActiveId(session.id);
    writeStoredValue(ACTIVE_SESSION_STORAGE_KEY, session.id);
    setFormOpen(false);
    setDrawerOpen(false);
  };

  const deleteSession = async (id: string) => {
    await api.deleteSession(id);
    const next = sessions.filter((session) => session.id !== id);
    setSessions(next);
    if (activeId === id) setActiveId(next[0]?.id ?? null);
  };

  const reconnectSession = async (id: string) => {
    setError(null);
    try {
      const session = await api.reconnectSession(id);
      updateSession(session);
      setSessionMenuId(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  };

  const updateSession = useCallback((nextSession: SessionSummary) => {
    setSessions((current) =>
      current.map((session) => (session.id === nextSession.id ? nextSession : session)),
    );
  }, []);

  return (
    <main className={`appShell ${sidebarCollapsed ? "sidebarCollapsed" : ""}`}>
      <aside className={`sidebar ${drawerOpen ? "open" : ""}`}>
        <div className="brand">
          <div className="brandMark">
            <Command size={18} />
          </div>
          <div>
            <h1>WebTerminal</h1>
            <p>Persistent tmux hub</p>
          </div>
          <button className="iconButton drawerClose" title="Close sessions" onClick={() => setDrawerOpen(false)}>
            <X size={17} />
          </button>
          <button
            className="iconButton sidebarCollapseButton"
            title={sidebarCollapsed ? "Expand sidebar" : "Collapse sidebar"}
            onClick={() => setSidebarCollapsed((collapsed) => !collapsed)}
          >
            {sidebarCollapsed ? <PanelLeftOpen size={17} /> : <PanelLeftClose size={17} />}
          </button>
        </div>

        <button className="primaryButton newSessionButton" title="New session" onClick={() => setFormOpen(true)}>
          <Plus size={17} />
          <span>New session</span>
        </button>

        <div className="sidebarHeader">
          <span>Sessions</span>
          <button className="iconButton" title="Refresh sessions" onClick={() => void refresh()}>
            <RefreshCw size={15} />
          </button>
        </div>

        <div className="sessionList">
          {sessions.length === 0 ? (
            <div className="emptyState">
              <Server size={18} />
              <span>No sessions yet</span>
            </div>
          ) : (
            sessions.map((session) => (
              <div
                key={session.id}
                className={`sessionItem ${session.id === activeId ? "active" : ""} ${sessionMenuId === session.id ? "menuOpen" : ""}`}
                title={`${session.name} · ${sessionModeLabel(session.mode)} · ${session.cols}x${session.rows}`}
              >
                <button
                  type="button"
                  className="sessionMain"
                  onClick={() => {
                    setActiveId(session.id);
                    setDrawerOpen(false);
                  }}
                >
                  <span className="sessionIcon">
                    {session.mode.includes("ssh") ? <Wifi size={16} /> : <Monitor size={16} />}
                  </span>
                  <span className="sessionText">
                    <strong>{session.name}</strong>
                    <small>
                      {session.alive ? sessionModeLabel(session.mode) : "disconnected"} · {session.cols}x{session.rows} ·{" "}
                      {session.viewers} viewers
                    </small>
                  </span>
                </button>
                <button
                  type="button"
                  className="sessionMenuToggle"
                  title="Session actions"
                  onClick={() =>
                    setSessionMenuId((current) => (current === session.id ? null : session.id))
                  }
                >
                  {sessionMenuId === session.id ? <ChevronDown size={15} /> : <ChevronRight size={15} />}
                </button>
                {sessionMenuId === session.id && (
                  <div className="sessionMenu">
                    <button type="button" onClick={() => void reconnectSession(session.id)}>
                      <RefreshCw size={14} />
                      Reconnect session
                    </button>
                  </div>
                )}
              </div>
            ))
          )}
        </div>
      </aside>
      {drawerOpen && <button className="drawerScrim" aria-label="Close sessions" onClick={() => setDrawerOpen(false)} />}

      <section className="workspace">
        <header className="topbar">
          <div className="topbarTitle">
            <button className="iconButton drawerToggle" title="Sessions" onClick={() => setDrawerOpen(true)}>
              <Menu size={17} />
            </button>
            <Circle className={active?.alive ? "liveDot" : "idleDot"} size={10} fill="currentColor" />
            <span>{active ? active.name : "No active terminal"}</span>
          </div>
          <div className="topbarActions">
            {active && (
              <>
                {sessionActionsOpen && (
                  <>
                    <span className="statusPill">{sessionModeLabel(active.mode)}</span>
                    <span className="statusPill">{active.cols}x{active.rows}</span>
                    <button
                      className="iconButton"
                      title="Reset PTY size to this view"
                      onClick={() => setResizeToken((current) => current + 1)}
                    >
                      <Maximize2 size={16} />
                    </button>
                    <button
                      className="iconButton danger"
                      title="Terminate session"
                      onClick={() => setDeleteTarget(active)}
                    >
                      <Trash2 size={16} />
                    </button>
                  </>
                )}
                <button
                  className="iconButton"
                  title={sessionActionsOpen ? "Hide session actions" : "Show session actions"}
                  onClick={() => setSessionActionsOpen((open) => !open)}
                >
                  {sessionActionsOpen ? <ChevronUp size={16} /> : <ChevronDown size={16} />}
                </button>
              </>
            )}
          </div>
        </header>

        {error && (
          <DismissibleMessage className="errorBanner" onClose={() => setError(null)}>
            {error}
          </DismissibleMessage>
        )}

        {active ? (
          <TerminalPane
            key={`${active.id}:${active.created_at}`}
            session={active}
            resizeToken={resizeToken}
            onResized={updateSession}
            onError={setError}
          />
        ) : (
          <div className="welcome">
            <div className="welcomeIcon">
              <Smartphone size={28} />
            </div>
            <h2>Create a terminal session</h2>
            <p>Start with tmux -CC, then reconnect from phone and desktop.</p>
            <button className="primaryButton compact" onClick={() => setFormOpen(true)}>
              <Plus size={17} />
              New session
            </button>
          </div>
        )}
      </section>

      {formOpen && (
        <CreateSessionDialog
          onClose={() => setFormOpen(false)}
          onCreate={createSession}
        />
      )}

      {deleteTarget && (
        <ConfirmDeleteDialog
          session={deleteTarget}
          onCancel={() => setDeleteTarget(null)}
          onConfirm={async () => {
            await deleteSession(deleteTarget.id);
            setDeleteTarget(null);
          }}
        />
      )}
    </main>
  );
}

function ConfirmDeleteDialog({
  session,
  onCancel,
  onConfirm,
}: {
  session: SessionSummary;
  onCancel: () => void;
  onConfirm: () => Promise<void>;
}) {
  const [submitting, setSubmitting] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);

  const confirm = async () => {
    if (submitting) return;
    setSubmitting(true);
    setFormError(null);
    try {
      await onConfirm();
    } catch (err) {
      setFormError(err instanceof Error ? err.message : String(err));
      setSubmitting(false);
    }
  };

  return (
    <div className="modalBackdrop">
      <div className="dialog confirmDialog" role="dialog" aria-modal="true">
        <div className="dialogHeader">
          <h2>Terminate session</h2>
          <button type="button" className="iconButton" onClick={onCancel} disabled={submitting}>
            x
          </button>
        </div>

        <p className="confirmText">
          This will terminate <strong>{session.name}</strong> and close its current PTY.
        </p>
        <div className="confirmMeta">{session.command}</div>
        {formError && (
          <DismissibleMessage className="formError" onClose={() => setFormError(null)}>
            {formError}
          </DismissibleMessage>
        )}

        <div className="dialogActions">
          <button type="button" className="secondaryButton" onClick={onCancel} disabled={submitting}>
            Cancel
          </button>
          <button className="dangerButton" type="button" onClick={() => void confirm()} disabled={submitting}>
            {submitting ? "Terminating..." : "Terminate"}
          </button>
        </div>
      </div>
    </div>
  );
}

function DismissibleMessage({
  className,
  children,
  onClose,
}: {
  className: string;
  children: React.ReactNode;
  onClose: () => void;
}) {
  return (
    <div className={className}>
      <span>{children}</span>
      <button type="button" className="messageClose" title="Close message" onClick={onClose}>
        <X size={14} />
      </button>
    </div>
  );
}

function TerminalPane({
  session,
  resizeToken,
  onResized,
  onError,
}: {
  session: SessionSummary;
  resizeToken: number;
  onResized: (session: SessionSummary) => void;
  onError: (message: string) => void;
}) {
  const containerRef = React.useRef<HTMLDivElement | null>(null);
  const desktopGridRef = React.useRef<HTMLDivElement | null>(null);
  const termRef = React.useRef<Terminal | null>(null);
  const paneTermsRef = React.useRef(new Map<string, CachedPaneTerminal>());
  const pendingPaneDataRef = React.useRef(new Map<string, Uint8Array[]>());
  const pendingPaneSnapshotRef = React.useRef(new Map<string, Uint8Array>());
  const wsRef = React.useRef<WebSocket | null>(null);
  const inputEnabledRef = React.useRef(true);
  const lastModeRef = React.useRef<InteractionMode>("input");
  const viewportRefreshTimerRef = React.useRef<number | null>(null);
  const wsReconnectTimerRef = React.useRef<number | null>(null);
  const zoomReplayTimerRef = React.useRef<number | null>(null);
  const handledResizeTokenRef = React.useRef(0);
  const focusedPaneRef = React.useRef<string | null>(null);
  const transferIdsRef = React.useRef(new Set<string>());
  const encoderRef = React.useRef(new TextEncoder());
  const [connected, setConnected] = useState(false);
  const [interactionMode, setInteractionMode] = useState<InteractionMode>("input");
  const [tmuxState, setTmuxState] = useState<TmuxState | null>(null);
  const [focusedPane, setFocusedPaneState] = useState<string | null>(() =>
    readStoredValue(focusedPaneStorageKey(session.id)),
  );
  const [toolsOpen, setToolsOpen] = useState(false);
  const [fileNotice, setFileNotice] = useState<string | null>(null);
  const controlMode = session.mode === "local_cc" || session.mode === "ssh_cc";
  const activePane = findActivePane(tmuxState, focusedPane);
  const activeWindow = findWindowForPane(tmuxState, activePane?.id ?? null);
  const visibleWindow = activeWindow ?? tmuxState?.windows[0] ?? null;
  const desktopSplitEnabled =
    controlMode && visibleWindow !== null && visibleWindow.panes.length > 1 && !visibleWindow.zoomed;
  const activeContext =
    activeWindow && activePane
      ? `${windowLabel(activeWindow)} / ${paneLabel(activePane)}`
      : session.name;

  const getCurrentTerm = useCallback(() => {
    if (controlMode) {
      const paneId = focusedPaneRef.current;
      return paneId ? (paneTermsRef.current.get(paneId)?.term ?? termRef.current) : termRef.current;
    }
    return termRef.current;
  }, [controlMode]);

  const ensurePaneTerminal = useCallback(
    (paneId: string): CachedPaneTerminal => {
      const cached = paneTermsRef.current.get(paneId);
      if (cached) return cached;

      const term = createTerminal(session.cols, session.rows, window.innerWidth < 720 ? 12 : 14);
      const element = document.createElement("div");
      element.className = "cachedPaneTerminal";
      attachTerminalKeyHandlers(term, (data) => {
        const ws = wsRef.current;
        if (inputEnabledRef.current && ws?.readyState === WebSocket.OPEN) {
          ws.send(encoderRef.current.encode(data));
        }
      });
      const dataSub = term.onData((data) => {
        const ws = wsRef.current;
        if (inputEnabledRef.current && ws?.readyState === WebSocket.OPEN) {
          ws.send(encoderRef.current.encode(data));
        }
      });
      const detachPasteHandler = attachPasteHandler(element, (data) => {
        const ws = wsRef.current;
        if (inputEnabledRef.current && ws?.readyState === WebSocket.OPEN) {
          ws.send(JSON.stringify({ type: "paste", data, pane_id: paneId }));
        }
      });
      const entry = { term, element, opened: false, dataSub, detachPasteHandler };
      paneTermsRef.current.set(paneId, entry);
      return entry;
    },
    [session.cols, session.rows],
  );

  const mountPaneTerminal = useCallback(
    (paneId: string, host: HTMLElement | null) => {
      if (!host) return null;
      const entry = ensurePaneTerminal(paneId);
      if (entry.element.parentElement !== host) {
        host.replaceChildren(entry.element);
      }
      if (!entry.opened) {
        entry.term.open(entry.element);
        entry.opened = true;
        const pending = pendingPaneSnapshotRef.current.has(paneId)
          ? null
          : pendingPaneDataRef.current.get(paneId);
        if (pending) {
          for (const chunk of pending) {
            writeTerminalData(entry.term, chunk, false);
          }
          pendingPaneDataRef.current.delete(paneId);
        }
      }
      termRef.current = entry.term;
      return entry.term;
    },
    [ensurePaneTerminal],
  );

  const flushPaneSnapshot = useCallback((paneId: string) => {
    pendingPaneSnapshotRef.current.delete(paneId);
    return true;
  }, []);

  const writePaneOutput = useCallback(
    (paneId: string, data: Uint8Array) => {
      if (pendingPaneSnapshotRef.current.has(paneId)) {
        const pending = pendingPaneDataRef.current.get(paneId) ?? [];
        pending.push(data);
        pendingPaneDataRef.current.set(paneId, pending);
        return;
      }
      const entry = paneTermsRef.current.get(paneId);
      if (!entry?.opened) {
        const pending = pendingPaneDataRef.current.get(paneId) ?? [];
        pending.push(data);
        pendingPaneDataRef.current.set(paneId, pending);
        return;
      }
      const shouldFollow = inputEnabledRef.current && focusedPaneRef.current === paneId;
      writeTerminalData(entry.term, data, shouldFollow);
    },
    [],
  );

  const replacePaneSnapshot = useCallback(
    (paneId: string, data: Uint8Array) => {
      void paneId;
      void data;
    },
    [],
  );

  const rememberFocusedPane = useCallback(
    (paneId: string | null) => {
      setFocusedPaneState(paneId);
      focusedPaneRef.current = paneId;
      writeStoredValue(focusedPaneStorageKey(session.id), paneId);
    },
    [session.id],
  );

  const resizeTerminalToContainer = useCallback(
    () => {
      const term = getCurrentTerm();
      const container = containerRef.current;
      if (!term || !container) return;

      const desktopGrid = desktopGridRef.current;
      const nextSize =
        desktopSplitEnabled && desktopGrid && isVisibleMeasureTarget(desktopGrid)
          ? measureDesktopGrid(desktopGrid, term)
          : measureTerminalGrid(container, term);
      if (
        controlMode &&
        desktopSplitEnabled &&
        desktopGrid &&
        !isVisibleMeasureTarget(desktopGrid) &&
        activePane &&
        visibleWindow &&
        !visibleWindow.zoomed
      ) {
        nextSize.zoom_pane_id = activePane.id;
      }
      const zoomPaneId = nextSize.zoom_pane_id;
      term.resize(nextSize.cols, nextSize.rows);
      if (inputEnabledRef.current && !zoomPaneId) {
        scrollTerminalToBottomSoon(term);
      }

      void api
        .resizeSession(session.id, nextSize)
        .then((updated) => {
          onResized(updated);
          if (!zoomPaneId) return;
          rememberFocusedPane(zoomPaneId);
          if (zoomReplayTimerRef.current !== null) {
            window.clearTimeout(zoomReplayTimerRef.current);
          }
          zoomReplayTimerRef.current = window.setTimeout(() => {
            zoomReplayTimerRef.current = null;
            const ws = wsRef.current;
            if (ws?.readyState === WebSocket.OPEN) {
              ws.send(JSON.stringify({ type: "focus_pane", pane_id: zoomPaneId, replay: false }));
            }
          }, 900);
        })
        .catch((err: unknown) => {
          onError(err instanceof Error ? err.message : String(err));
        });
    },
    [
      activePane,
      controlMode,
      desktopSplitEnabled,
      getCurrentTerm,
      session.id,
      onResized,
      onError,
      rememberFocusedPane,
      visibleWindow,
    ],
  );

  const scheduleLocalViewportRefresh = useCallback((delay = 120) => {
    if (viewportRefreshTimerRef.current !== null) {
      window.clearTimeout(viewportRefreshTimerRef.current);
    }
    viewportRefreshTimerRef.current = window.setTimeout(() => {
      const term = getCurrentTerm();
      viewportRefreshTimerRef.current = null;
      if (!term) return;
      term.resize(term.cols, term.rows);
      if (inputEnabledRef.current) {
        scrollTerminalToBottomSoon(term);
      }
    }, delay);
  }, [getCurrentTerm]);

  useEffect(() => {
    return () => {
      if (viewportRefreshTimerRef.current !== null) {
        window.clearTimeout(viewportRefreshTimerRef.current);
        viewportRefreshTimerRef.current = null;
      }
      if (wsReconnectTimerRef.current !== null) {
        window.clearTimeout(wsReconnectTimerRef.current);
        wsReconnectTimerRef.current = null;
      }
      if (zoomReplayTimerRef.current !== null) {
        window.clearTimeout(zoomReplayTimerRef.current);
        zoomReplayTimerRef.current = null;
      }
    };
  }, []);

  useEffect(() => {
    focusedPaneRef.current = focusedPane;
  }, [focusedPane]);

  useEffect(() => {
    const paneExists =
      tmuxState?.windows.some((window) => window.panes.some((pane) => pane.id === focusedPane)) ?? false;
    if (!controlMode || !focusedPane || !paneExists || desktopSplitEnabled) return;
    const term = mountPaneTerminal(focusedPane, containerRef.current);
    if (!term) return;
    term.options.disableStdin = interactionMode === "browse";
    term.resize(session.cols, session.rows);
    flushPaneSnapshot(focusedPane);
    if (interactionMode === "input") {
      scrollTerminalToBottomSoon(term);
      term.focus();
    }
  }, [
    controlMode,
    desktopSplitEnabled,
    focusedPane,
    interactionMode,
    flushPaneSnapshot,
    mountPaneTerminal,
    session.cols,
    session.rows,
    tmuxState,
  ]);

  useEffect(() => {
    setToolsOpen(false);
  }, [session.id]);

  useEffect(() => {
    const term = createTerminal(session.cols, session.rows, window.innerWidth < 720 ? 12 : 14);
    attachTerminalKeyHandlers(term, (data) => {
      const ws = wsRef.current;
      if (inputEnabledRef.current && ws?.readyState === WebSocket.OPEN) {
        ws.send(encoderRef.current.encode(data));
      }
    });
    termRef.current = term;

    if (!containerRef.current) return;
    containerRef.current.replaceChildren();
    term.open(containerRef.current);
    const detachPasteHandler = attachPasteHandler(containerRef.current, (data) => {
      const ws = wsRef.current;
      if (inputEnabledRef.current && ws?.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify({ type: "paste", data, pane_id: focusedPaneRef.current }));
      }
    });
    scheduleLocalViewportRefresh(0);

    let disposed = false;
    let reconnectAttempt = 0;

    const clearReconnectTimer = () => {
      if (wsReconnectTimerRef.current !== null) {
        window.clearTimeout(wsReconnectTimerRef.current);
        wsReconnectTimerRef.current = null;
      }
    };

    const scheduleReconnect = () => {
      if (disposed || document.hidden) return;
      clearReconnectTimer();
      const delay = Math.min(1000 * 2 ** reconnectAttempt, 10000);
      reconnectAttempt += 1;
      wsReconnectTimerRef.current = window.setTimeout(() => {
        wsReconnectTimerRef.current = null;
        connectWebSocket();
      }, delay);
    };

    const connectWebSocket = () => {
      if (disposed) return;
      const current = wsRef.current;
      if (
        current &&
        (current.readyState === WebSocket.OPEN || current.readyState === WebSocket.CONNECTING)
      ) {
        return;
      }

      const proto = window.location.protocol === "https:" ? "wss" : "ws";
      const ws = new WebSocket(`${proto}://${window.location.host}/ws/sessions/${session.id}`);
      ws.binaryType = "arraybuffer";
      wsRef.current = ws;

      ws.onopen = () => {
        if (disposed || wsRef.current !== ws) return;
        reconnectAttempt = 0;
        setConnected(true);
        term.clear();
        term.focus();
        if (inputEnabledRef.current) {
          scrollTerminalToBottomSoon(term);
        }
        const paneId = focusedPaneRef.current;
        if (paneId) {
          ws.send(JSON.stringify({ type: "focus_pane", pane_id: paneId, replay: false }));
        }
      };
      ws.onclose = () => {
        if (wsRef.current === ws) {
          wsRef.current = null;
        }
        setConnected(false);
        scheduleReconnect();
      };
      ws.onerror = () => {
        setConnected(false);
      };
      ws.onmessage = (event) => {
        if (disposed || wsRef.current !== ws) return;
        if (event.data instanceof ArrayBuffer) {
          writeTerminalData(term, new Uint8Array(event.data), inputEnabledRef.current);
        } else if (event.data instanceof Blob) {
          void event.data.arrayBuffer().then((buffer) => {
            if (!disposed && wsRef.current === ws) {
              writeTerminalData(term, new Uint8Array(buffer), inputEnabledRef.current);
            }
          });
        } else {
          const terminalChanged = handleServerTextMessage(
            String(event.data),
            term,
            setTmuxState,
            rememberFocusedPane,
            writePaneOutput,
            replacePaneSnapshot,
            (message) => {
              if (!transferIdsRef.current.has(message.id)) return;
              transferIdsRef.current.delete(message.id);
              const filename = decodeBase64Text(message.filename_base64);
              triggerBrowserDownload(filename, message.data_base64);
              setFileNotice(`Downloaded ${filename}`);
            },
            (message) => {
              if (!transferIdsRef.current.has(message.id)) return;
              if (message.status === "error") {
                transferIdsRef.current.delete(message.id);
              }
              setFileNotice(message.message);
            },
          );
          if (terminalChanged && inputEnabledRef.current) {
            if (terminalIsNearBottom(term)) {
              scrollTerminalToBottomSoon(term);
            }
          }
        }
      };
    };

    const restoreInputViewport = () => {
      if (document.hidden) return;
      if (inputEnabledRef.current) {
        scrollTerminalToBottomSoon(term);
      }
    };

    const reconnectWhenVisible = () => {
      restoreInputViewport();
      const ws = wsRef.current;
      if (!ws || ws.readyState === WebSocket.CLOSED || ws.readyState === WebSocket.CLOSING) {
        reconnectAttempt = 0;
        clearReconnectTimer();
        connectWebSocket();
      }
    };

    connectWebSocket();
    document.addEventListener("visibilitychange", reconnectWhenVisible);
    window.addEventListener("focus", restoreInputViewport);
    window.addEventListener("online", reconnectWhenVisible);

    const dataSub = term.onData((data) => {
      const ws = wsRef.current;
      if (inputEnabledRef.current && ws?.readyState === WebSocket.OPEN) {
        ws.send(encoderRef.current.encode(data));
      }
    });

    return () => {
      disposed = true;
      document.removeEventListener("visibilitychange", reconnectWhenVisible);
      window.removeEventListener("focus", restoreInputViewport);
      window.removeEventListener("online", reconnectWhenVisible);
      clearReconnectTimer();
      if (viewportRefreshTimerRef.current !== null) {
        window.clearTimeout(viewportRefreshTimerRef.current);
        viewportRefreshTimerRef.current = null;
      }
      if (zoomReplayTimerRef.current !== null) {
        window.clearTimeout(zoomReplayTimerRef.current);
        zoomReplayTimerRef.current = null;
      }
      dataSub.dispose();
      detachPasteHandler();
      wsRef.current?.close();
      term.dispose();
      for (const entry of paneTermsRef.current.values()) {
        entry.dataSub.dispose();
        entry.detachPasteHandler();
        entry.term.dispose();
      }
      paneTermsRef.current.clear();
      pendingPaneDataRef.current.clear();
      pendingPaneSnapshotRef.current.clear();
      termRef.current = null;
      wsRef.current = null;
    };
  }, [
    session.id,
    scheduleLocalViewportRefresh,
    rememberFocusedPane,
    writePaneOutput,
    replacePaneSnapshot,
  ]);

  useEffect(() => {
    if (!tmuxState) return;
    const panes = tmuxState.windows.flatMap((window) => window.panes);
    if (panes.length === 0) {
      if (focusedPane) {
        rememberFocusedPane(null);
      }
      return;
    }
    if (focusedPane && panes.some((pane) => pane.id === focusedPane)) return;

    const storedPane = readStoredValue(focusedPaneStorageKey(session.id));
    const nextPane =
      (storedPane && panes.find((pane) => pane.id === storedPane)?.id) ??
      tmuxState.active_pane ??
      panes[0]?.id ??
      null;
    if (nextPane) {
      rememberFocusedPane(nextPane);
    }
  }, [focusedPane, rememberFocusedPane, session.id, tmuxState]);

  useEffect(() => {
    inputEnabledRef.current = interactionMode === "input";
    const term = getCurrentTerm();
    const ws = wsRef.current;
    if (!term) return;

    term.options.disableStdin = interactionMode === "browse";
    if (interactionMode === "input") {
      if (!controlMode && lastModeRef.current === "browse" && ws?.readyState === WebSocket.OPEN) {
        ws.send("\x1b");
      }
      scrollTerminalToBottomSoon(term);
      term.focus();
    } else {
      if (!controlMode && lastModeRef.current === "input" && ws?.readyState === WebSocket.OPEN) {
        ws.send("\x02[");
      }
      term.blur();
    }
    lastModeRef.current = interactionMode;
  }, [controlMode, getCurrentTerm, interactionMode]);

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    let lastTouchY: number | null = null;
    let scrollRemainder = 0;
    let wheelRemainder = 0;
    const lineThreshold = 12;

    const sendTmuxScroll = (deltaY: number) => {
      if (controlMode || interactionMode !== "browse") return;
      const ws = wsRef.current;
      if (!ws || ws.readyState !== WebSocket.OPEN) return;

      wheelRemainder += deltaY;
      const lines = Math.trunc(wheelRemainder / lineThreshold);
      if (lines === 0) return;

      wheelRemainder -= lines * lineThreshold;
      const sequence = lines < 0 ? "\x1b[A" : "\x1b[B";
      ws.send(sequence.repeat(Math.min(Math.abs(lines), 20)));
    };

    const scrollHistory = (deltaY: number) => {
      const term = getCurrentTerm();
      if (!term) return;
      scrollRemainder += deltaY;
      const lines = Math.trunc(scrollRemainder / lineThreshold);
      if (lines !== 0) {
        scrollRemainder -= lines * lineThreshold;
        term.scrollLines(lines);
      }
      sendTmuxScroll(deltaY);
    };

    const onWheel = (event: WheelEvent) => {
      event.preventDefault();
      scrollHistory(event.deltaY);
    };

    const onTouchStart = (event: TouchEvent) => {
      lastTouchY = event.touches.length === 1 ? event.touches[0].clientY : null;
    };

    const onTouchMove = (event: TouchEvent) => {
      if (lastTouchY === null || event.touches.length !== 1) return;
      const nextY = event.touches[0].clientY;
      scrollHistory(lastTouchY - nextY);
      lastTouchY = nextY;
      event.preventDefault();
    };

    const onTouchEnd = () => {
      lastTouchY = null;
    };

    container.addEventListener("wheel", onWheel, { capture: true, passive: false });
    container.addEventListener("touchstart", onTouchStart, { capture: true, passive: true });
    container.addEventListener("touchmove", onTouchMove, { capture: true, passive: false });
    container.addEventListener("touchend", onTouchEnd, { capture: true });
    container.addEventListener("touchcancel", onTouchEnd, { capture: true });

    return () => {
      container.removeEventListener("wheel", onWheel, { capture: true });
      container.removeEventListener("touchstart", onTouchStart, { capture: true });
      container.removeEventListener("touchmove", onTouchMove, { capture: true });
      container.removeEventListener("touchend", onTouchEnd, { capture: true });
      container.removeEventListener("touchcancel", onTouchEnd, { capture: true });
    };
  }, [controlMode, getCurrentTerm, interactionMode]);

  useEffect(() => {
    const refreshTerminalViewport = () => scheduleLocalViewportRefresh();

    const viewport = window.visualViewport;
    viewport?.addEventListener("resize", refreshTerminalViewport);
    viewport?.addEventListener("scroll", refreshTerminalViewport);
    window.addEventListener("resize", refreshTerminalViewport);
    window.addEventListener("orientationchange", refreshTerminalViewport);

    return () => {
      viewport?.removeEventListener("resize", refreshTerminalViewport);
      viewport?.removeEventListener("scroll", refreshTerminalViewport);
      window.removeEventListener("resize", refreshTerminalViewport);
      window.removeEventListener("orientationchange", refreshTerminalViewport);
      if (viewportRefreshTimerRef.current !== null) {
        window.clearTimeout(viewportRefreshTimerRef.current);
        viewportRefreshTimerRef.current = null;
      }
    };
  }, [scheduleLocalViewportRefresh]);

  useEffect(() => {
    if (resizeToken === 0) return;
    if (handledResizeTokenRef.current === resizeToken) return;
    handledResizeTokenRef.current = resizeToken;
    resizeTerminalToContainer();
  }, [resizeToken, resizeTerminalToContainer]);

  const send = (value: string) => {
    if (interactionMode !== "input") return;
    const ws = wsRef.current;
    if (ws?.readyState === WebSocket.OPEN) ws.send(encoderRef.current.encode(value));
  };

  const focusPane = (paneId: string) => {
    const ws = wsRef.current;
    if (ws?.readyState !== WebSocket.OPEN) return;
    const targetWindow = findWindowForPane(tmuxState, paneId);
    const shouldZoomBeforeFocus =
      controlMode &&
      targetWindow !== null &&
      targetWindow.panes.length > 1 &&
      !targetWindow.zoomed &&
      !window.matchMedia("(min-width: 761px)").matches;

    setInteractionMode("input");
    rememberFocusedPane(paneId);
    mountPaneTerminal(paneId, desktopSplitEnabled ? null : containerRef.current);
    if (shouldZoomBeforeFocus) {
      ws.send(JSON.stringify({ type: "tmux_command", command: "zoom_pane", pane_id: paneId }));
      if (zoomReplayTimerRef.current !== null) {
        window.clearTimeout(zoomReplayTimerRef.current);
      }
      zoomReplayTimerRef.current = window.setTimeout(() => {
        zoomReplayTimerRef.current = null;
        const currentWs = wsRef.current;
        if (currentWs?.readyState === WebSocket.OPEN) {
          currentWs.send(JSON.stringify({ type: "focus_pane", pane_id: paneId, replay: false }));
        }
      }, 900);
      return;
    }
    ws.send(JSON.stringify({ type: "focus_pane", pane_id: paneId, replay: false }));
  };

  const sendTmuxCommand = (command: TmuxCommand) => {
    const ws = wsRef.current;
    if (ws?.readyState !== WebSocket.OPEN) return;
    ws.send(JSON.stringify({ type: "tmux_command", command, pane_id: activePane?.id ?? null }));
  };

  const resizePane = (paneId: string, direction: ResizeDirection, amount: number) => {
    const ws = wsRef.current;
    if (ws?.readyState !== WebSocket.OPEN) return;
    ws.send(
      JSON.stringify({
        type: "resize_pane",
        pane_id: paneId,
        direction,
        amount: clamp(Math.round(amount), 1, 80),
      }),
    );
  };

  const renameWindow = (windowId: string, current: string) => {
    const ws = wsRef.current;
    if (ws?.readyState !== WebSocket.OPEN) return;
    const next = window.prompt("Tab name", current);
    if (next === null) return;
    const name = next.trim();
    if (!name || name === current) return;
    ws.send(JSON.stringify({ type: "rename_window", window_id: windowId, name }));
  };

  const setPaneNote = (paneId: string, current: string) => {
    const ws = wsRef.current;
    if (ws?.readyState !== WebSocket.OPEN) return;
    const next = window.prompt("Pane note", current);
    if (next === null) return;
    ws.send(JSON.stringify({ type: "set_pane_note", pane_id: paneId, note: next.trim() }));
  };

  const copySelection = async () => {
    const selection = getCurrentTerm()?.getSelection() ?? "";
    if (!selection) return;
    await navigator.clipboard.writeText(selection);
  };

  const downloadRemoteFile = () => {
    const ws = wsRef.current;
    if (ws?.readyState !== WebSocket.OPEN) return;
    const path = window.prompt("Remote file path to download", "");
    if (!path?.trim()) return;
    const id = makeTransferId();
    transferIdsRef.current.add(id);
    setFileNotice(`Downloading ${path.trim()}...`);
    ws.send(
      JSON.stringify({
        type: "file_download",
        id,
        path: path.trim(),
        pane_id: focusedPaneRef.current,
      }),
    );
  };

  const uploadRemoteFile = async () => {
    const ws = wsRef.current;
    if (ws?.readyState !== WebSocket.OPEN) return;
    const file = await chooseLocalFile();
    if (!file) return;
    if (file.size > MAX_UPLOAD_BYTES) {
      setFileNotice(`Upload is limited to ${Math.floor(MAX_UPLOAD_BYTES / 1024 / 1024)} MB in this version`);
      return;
    }

    const defaultPath = `./${file.name}`;
    const path = window.prompt("Remote destination path", defaultPath);
    if (!path?.trim()) return;

    const id = makeTransferId();
    const paneId = focusedPaneRef.current;
    transferIdsRef.current.add(id);
    setFileNotice(`Uploading ${file.name}...`);

    try {
      const base64 = await readFileAsBase64(file);
      ws.send(JSON.stringify({ type: "file_upload_start", id, path: path.trim(), pane_id: paneId }));
      for (let offset = 0; offset < base64.length; offset += FILE_UPLOAD_CHUNK_CHARS) {
        if (ws.readyState !== WebSocket.OPEN) throw new Error("terminal connection closed");
        ws.send(
          JSON.stringify({
            type: "file_upload_chunk",
            id,
            data: base64.slice(offset, offset + FILE_UPLOAD_CHUNK_CHARS),
          }),
        );
        await waitForSocketBuffer(ws);
      }
      ws.send(JSON.stringify({ type: "file_upload_finish", id }));
      setFileNotice(`Upload sent: ${path.trim()}`);
      window.setTimeout(() => transferIdsRef.current.delete(id), 30_000);
    } catch (err) {
      transferIdsRef.current.delete(id);
      setFileNotice(err instanceof Error ? err.message : String(err));
      if (ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify({ type: "file_transfer_cancel", id }));
      }
    }
  };

  return (
    <div className="terminalWrap">
      <div className="terminalChrome compact">
        <span className={`connectionBadge ${connected ? "connected" : "disconnected"}`}>
          {connected ? "on" : "off"}
        </span>
        <span className="terminalContext">{activeContext}</span>
        <ModeSwitch mode={interactionMode} onChange={setInteractionMode} />
        <button
          className="iconButton compactIcon"
          type="button"
          title={toolsOpen ? "Hide tmux controls" : "Show tmux controls"}
          onClick={() => setToolsOpen((open) => !open)}
        >
          {toolsOpen ? <ChevronUp size={15} /> : <ChevronDown size={15} />}
        </button>
      </div>
      {toolsOpen && (
        <div className="terminalTools">
          <button className="chromeButton" type="button" onClick={() => void copySelection()}>
            <Copy size={14} />
            Copy
          </button>
          <button className="chromeButton" type="button" onClick={downloadRemoteFile}>
            <Download size={14} />
            Download
          </button>
          <button className="chromeButton" type="button" onClick={() => void uploadRemoteFile()}>
            <Upload size={14} />
            Upload
          </button>
          <span className="terminalCommand">{session.command}</span>
        </div>
      )}
      {fileNotice && (
        <DismissibleMessage className="fileNotice" onClose={() => setFileNotice(null)}>
          {fileNotice}
        </DismissibleMessage>
      )}
      {controlMode && tmuxState && toolsOpen && (
        <TmuxNavigator
          state={tmuxState}
          focusedPane={focusedPane}
          onFocusPane={focusPane}
          onCommand={sendTmuxCommand}
          onRenameWindow={renameWindow}
          onSetPaneNote={setPaneNote}
        />
      )}
      <div
        ref={containerRef}
        className={`terminalSurface ${interactionMode === "browse" ? "browseMode" : "inputMode"} ${desktopSplitEnabled ? "desktopSplitHidden" : ""}`}
      />
      {desktopSplitEnabled && visibleWindow && (
        <DesktopPaneGrid
          ref={desktopGridRef}
          window={visibleWindow}
          focusedPane={focusedPane}
          interactionMode={interactionMode}
          flushPaneSnapshot={flushPaneSnapshot}
          mountPaneTerminal={mountPaneTerminal}
          onFocusPane={focusPane}
          onResizePane={resizePane}
        />
      )}
      <MobileKeybar
        mode={interactionMode}
        onCopy={() => void copySelection()}
        onSend={send}
      />
    </div>
  );
}

function measureTerminalGrid(container: HTMLElement, term: Terminal): ResizeSessionPayload {
  const styles = window.getComputedStyle(container);
  const paddingX = parseFloat(styles.paddingLeft) + parseFloat(styles.paddingRight);
  const paddingY = parseFloat(styles.paddingTop) + parseFloat(styles.paddingBottom);
  const verticalScrollbarWidth = Math.max(container.offsetWidth - container.clientWidth, 0);
  const width =
    container.clientWidth -
    paddingX -
    verticalScrollbarWidth -
    TERMINAL_SCROLLBAR_GUARD_PX -
    TERMINAL_FIT_GUARD_PX;
  const height = container.clientHeight - paddingY;
  const screen = container.querySelector<HTMLElement>(".xterm-screen");
  const rect = screen?.getBoundingClientRect();
  const cellWidth =
    rect && rect.width > 0 && term.cols > 0 ? rect.width / term.cols : getFallbackCellWidth(term);
  const cellHeight =
    rect && rect.height > 0 && term.rows > 0 ? rect.height / term.rows : getFallbackCellHeight(term);

  return {
    cols: clamp(Math.floor(width / cellWidth), 40, 240),
    rows: clamp(Math.floor(height / cellHeight), 12, 80),
  };
}

function isVisibleMeasureTarget(element: HTMLElement): boolean {
  if (element.clientWidth <= 0 || element.clientHeight <= 0) return false;
  const styles = window.getComputedStyle(element);
  return styles.display !== "none" && styles.visibility !== "hidden";
}

function measureDesktopGrid(grid: HTMLElement, fallbackTerm: Terminal): ResizeSessionPayload {
  const styles = window.getComputedStyle(grid);
  const parent = grid.parentElement;
  const availableWidth = parent?.clientWidth ?? grid.clientWidth;
  const availableHeight = parent
    ? Math.max(parent.clientHeight - grid.offsetTop, 0)
    : grid.clientHeight;
  const width = availableWidth - parseFloat(styles.paddingLeft) - parseFloat(styles.paddingRight);
  const height = availableHeight - parseFloat(styles.paddingTop) - parseFloat(styles.paddingBottom);
  const surfaces = Array.from(
    grid.querySelectorAll<HTMLElement>(".desktopPaneSurface[data-cols][data-rows]"),
  );
  const surface = surfaces[0];
  const screen = surface?.querySelector<HTMLElement>(".xterm-screen");
  const rect = screen?.getBoundingClientRect();
  const cols = Number(surface?.dataset.cols ?? 0);
  const rows = Number(surface?.dataset.rows ?? 0);
  const measuredCellWidth =
    rect && rect.width > 0 && cols > 0
      ? rect.width / cols
      : null;
  const measuredCellHeight =
    rect && rect.height > 0 && rows > 0
      ? rect.height / rows
      : null;
  const cellWidth = Math.max(measuredCellWidth ?? 0, getFallbackCellWidth(fallbackTerm));
  const cellHeight = Math.max(measuredCellHeight ?? 0, getFallbackCellHeight(fallbackTerm));
  const verticalBoundaries = new Set(
    surfaces
      .map((item) => Number(item.dataset.paneLeft ?? 0))
      .filter((left) => Number.isFinite(left) && left > 0),
  );
  const reservedWidth =
    verticalBoundaries.size * DESKTOP_PANE_SEPARATOR_GAP_PX +
    DESKTOP_PANE_EDGE_GUARD_PX +
    TERMINAL_FIT_GUARD_PX +
    2;
  const safetyRows = surfaces.length > 1 ? 1 : 0;

  return {
    cols: clamp(Math.floor((width - reservedWidth) / cellWidth), 40, 240),
    rows: clamp(Math.floor(height / cellHeight) - safetyRows, 12, 80),
  };
}

function handleServerTextMessage(
  data: string,
  term: Terminal,
  setTmuxState: React.Dispatch<React.SetStateAction<TmuxState | null>>,
  setFocusedPane: (paneId: string) => void,
  onPaneOutput: (paneId: string, data: Uint8Array) => void,
  onPaneSnapshot: (paneId: string, data: Uint8Array) => void,
  onFileDownload: (message: Extract<ServerMessage, { type: "file_download" }>) => void,
  onFileTransferStatus: (message: Extract<ServerMessage, { type: "file_transfer_status" }>) => void,
): boolean {
  let message: ServerMessage;
  try {
    message = JSON.parse(data) as ServerMessage;
  } catch {
    term.write(data);
    return true;
  }

  if (message.type === "clear") {
    term.clear();
    return true;
  } else if (message.type === "focus_pane") {
    setFocusedPane(message.pane_id);
  } else if (message.type === "pane_output") {
    onPaneOutput(message.pane_id, base64ToBytes(message.data_base64));
    return false;
  } else if (message.type === "pane_snapshot") {
    onPaneSnapshot(message.pane_id, base64ToBytes(message.data_base64));
    return false;
  } else if (message.type === "tmux_state") {
    setTmuxState(message.state);
  } else if (message.type === "file_download") {
    onFileDownload(message);
  } else if (message.type === "file_transfer_status") {
    onFileTransferStatus(message);
  }
  return false;
}

function findActivePane(state: TmuxState | null, focusedPane: string | null): TmuxPane | null {
  const panes = state?.windows.flatMap((window) => window.panes) ?? [];
  return (
    panes.find((pane) => pane.id === focusedPane) ??
    panes.find((pane) => pane.id === state?.active_pane) ??
    panes[0] ??
    null
  );
}

function findWindowForPane(state: TmuxState | null, paneId: string | null): TmuxWindow | null {
  if (!state || !paneId) return null;
  return state.windows.find((window) => window.panes.some((pane) => pane.id === paneId)) ?? null;
}

function windowLabel(window: TmuxWindow): string {
  return `${window.index ?? "-"} ${window.name || window.id}`;
}

function paneLabel(pane: TmuxPane): string {
  return pane.note || `${pane.id} ${pane.current_command || "shell"}`;
}

const DesktopPaneGrid = React.forwardRef<HTMLDivElement, {
  window: TmuxWindow;
  focusedPane: string | null;
  interactionMode: InteractionMode;
  flushPaneSnapshot: (paneId: string) => boolean;
  mountPaneTerminal: (paneId: string, host: HTMLElement | null) => Terminal | null;
  onFocusPane: (paneId: string) => void;
  onResizePane: (paneId: string, direction: ResizeDirection, amount: number) => void;
}>(function DesktopPaneGrid({
  window,
  focusedPane,
  interactionMode,
  flushPaneSnapshot,
  mountPaneTerminal,
  onFocusPane,
  onResizePane,
}, ref) {
  const [cellSize, setCellSize] = useState<{ width: number; height: number } | null>(null);
  const [gridSize, setGridSize] = useState<{ width: number; height: number } | null>(null);
  const gridRef = React.useRef<HTMLDivElement | null>(null);
  const hasGeometry = window.panes.every(
    (pane) => pane.left !== null && pane.top !== null && pane.width !== null && pane.height !== null,
  );
  const maxRight = Math.max(
    ...window.panes.map((pane) => (pane.left ?? 0) + (pane.width ?? 1)),
    1,
  );
  const maxBottom = Math.max(
    ...window.panes.map((pane) => (pane.top ?? 0) + (pane.height ?? 1)),
    1,
  );
  const verticalBoundaries = Array.from(
    new Set(
      window.panes
        .map((pane) => pane.left)
        .filter((left): left is number => left != null && left > 0),
    ),
  ).sort((a, b) => a - b);
  const rememberCellSize = useCallback((width: number, height: number) => {
    if (!Number.isFinite(width) || !Number.isFinite(height) || width <= 0 || height <= 0) return;
    setCellSize((current) => {
      if (
        current &&
        Math.abs(current.width - width) < 0.05 &&
        Math.abs(current.height - height) < 0.05
      ) {
        return current;
      }
      return { width, height };
    });
  }, []);
  const setGridRefs = useCallback(
    (node: HTMLDivElement | null) => {
      gridRef.current = node;
      if (typeof ref === "function") {
        ref(node);
      } else if (ref) {
        ref.current = node;
      }
    },
    [ref],
  );

  useEffect(() => {
    const node = gridRef.current;
    if (!node) return;
    const parent = node.parentElement;

    const updateGridSize = () => {
      const width = parent?.clientWidth ?? node.clientWidth;
      const height = parent
        ? Math.max(parent.clientHeight - node.offsetTop, 0)
        : node.clientHeight;
      if (width <= 0 || height <= 0) return;
      setGridSize((current) => {
        if (current && current.width === width && current.height === height) return current;
        return { width, height };
      });
    };

    updateGridSize();
    const observer = new ResizeObserver(updateGridSize);
    observer.observe(parent ?? node);
    return () => observer.disconnect();
  }, []);

  const horizontalReserve =
    verticalBoundaries.length * DESKTOP_PANE_SEPARATOR_GAP_PX + DESKTOP_PANE_EDGE_GUARD_PX + 2;
  const effectiveCellSize =
    cellSize && gridSize
      ? {
          width: Math.max(1, Math.min(cellSize.width, (gridSize.width - horizontalReserve) / maxRight)),
          height: Math.max(1, Math.min(cellSize.height, (gridSize.height - 2) / maxBottom)),
        }
      : cellSize;
  const pixelGeometry = hasGeometry && effectiveCellSize !== null && gridSize !== null;
  const gridStyle: React.CSSProperties | undefined = pixelGeometry
    ? {
        width: `${
          Math.ceil(maxRight * effectiveCellSize.width) + horizontalReserve
        }px`,
        height: `${Math.ceil(maxBottom * effectiveCellSize.height) + 2}px`,
      }
    : undefined;

  return (
    <div
      ref={setGridRefs}
      className={`desktopPaneGrid ${hasGeometry ? "geometry" : "fallback"} ${pixelGeometry ? "pixelGeometry" : ""}`}
      style={gridStyle}
    >
      {window.panes.map((pane) => {
        const left = pane.left ?? 0;
        const top = pane.top ?? 0;
        const width = pane.width ?? maxRight;
        const height = pane.height ?? maxBottom;
        const boundariesBefore = verticalBoundaries.filter((boundary) => boundary <= left).length;
        const style: React.CSSProperties = hasGeometry
          ? pixelGeometry
            ? {
                left: `${Math.round(left * effectiveCellSize.width) + boundariesBefore * DESKTOP_PANE_SEPARATOR_GAP_PX}px`,
                top: `${Math.round(top * effectiveCellSize.height)}px`,
                width: `${Math.ceil(width * effectiveCellSize.width) + DESKTOP_PANE_EDGE_GUARD_PX + 2}px`,
                height: `${Math.ceil(height * effectiveCellSize.height) + 2}px`,
              }
            : {
                left: `${(left / maxRight) * 100}%`,
                top: `${(top / maxBottom) * 100}%`,
                width: `${(width / maxRight) * 100}%`,
                height: `${(height / maxBottom) * 100}%`,
              }
          : {};

        return (
          <PaneTerminal
            key={pane.id}
            pane={pane}
            selected={pane.id === focusedPane}
            interactionMode={interactionMode}
            style={style}
            flushPaneSnapshot={flushPaneSnapshot}
            mountPaneTerminal={mountPaneTerminal}
            onFocus={() => onFocusPane(pane.id)}
            onResizePane={onResizePane}
            onCellSize={rememberCellSize}
          />
        );
      })}
    </div>
  );
});

function PaneTerminal({
  pane,
  selected,
  interactionMode,
  style,
  flushPaneSnapshot,
  mountPaneTerminal,
  onFocus,
  onResizePane,
  onCellSize,
}: {
  pane: TmuxPane;
  selected: boolean;
  interactionMode: InteractionMode;
  style: React.CSSProperties;
  flushPaneSnapshot: (paneId: string) => boolean;
  mountPaneTerminal: (paneId: string, host: HTMLElement | null) => Terminal | null;
  onFocus: () => void;
  onResizePane: (paneId: string, direction: ResizeDirection, amount: number) => void;
  onCellSize: (width: number, height: number) => void;
}) {
  const containerRef = React.useRef<HTMLDivElement | null>(null);
  const termRef = React.useRef<Terminal | null>(null);
  const dragRef = React.useRef<{
    x: number;
    y: number;
    axis: "x" | "y";
    pointerId: number;
  } | null>(null);

  useEffect(() => {
    const term = mountPaneTerminal(pane.id, containerRef.current);
    termRef.current = term;
    if (!term) return;

    const resizeToPane = () => {
      const container = containerRef.current;
      if (!container) return;
      const cols = clamp(pane.width ?? 80, 1, 240);
      const rows = clamp(pane.height ?? 24, 1, 80);
      term.resize(cols, rows);
      flushPaneSnapshot(pane.id);
      container.dataset.cols = String(cols);
      container.dataset.rows = String(rows);
      container.dataset.paneLeft = String(pane.left ?? 0);
      container.dataset.paneTop = String(pane.top ?? 0);
      const screen = container.querySelector<HTMLElement>(".xterm-screen");
      const rect = screen?.getBoundingClientRect();
      if (rect && rect.width > 0 && rect.height > 0) {
        container.dataset.cellWidth = String(rect.width / cols);
        container.dataset.cellHeight = String(rect.height / rows);
        onCellSize(rect.width / cols, rect.height / rows);
      }
    };

    window.setTimeout(resizeToPane, 0);
    window.setTimeout(resizeToPane, 120);
  }, [
    flushPaneSnapshot,
    mountPaneTerminal,
    onCellSize,
    pane.height,
    pane.id,
    pane.left,
    pane.top,
    pane.width,
  ]);

  useEffect(() => {
    const term = termRef.current;
    if (!term) return;
    term.options.disableStdin = interactionMode === "browse";
    if (selected && interactionMode === "input") {
      term.focus();
      scrollTerminalToBottomSoon(term);
    }
  }, [interactionMode, selected]);

  useEffect(() => {
    const container = containerRef.current;
    const term = termRef.current;
    if (!container || !term) return;

    let lastTouchY: number | null = null;
    let scrollRemainder = 0;
    const lineThreshold = 12;

    const scrollHistory = (deltaY: number) => {
      const currentTerm = termRef.current;
      if (!currentTerm) return;
      scrollRemainder += deltaY;
      const lines = Math.trunc(scrollRemainder / lineThreshold);
      if (lines !== 0) {
        scrollRemainder -= lines * lineThreshold;
        currentTerm.scrollLines(lines);
      }
    };

    const onWheel = (event: WheelEvent) => {
      if ((event.target as HTMLElement | null)?.closest(".paneResizeHandle")) return;
      event.preventDefault();
      scrollHistory(event.deltaY);
    };

    const onTouchStart = (event: TouchEvent) => {
      if ((event.target as HTMLElement | null)?.closest(".paneResizeHandle")) return;
      lastTouchY = event.touches.length === 1 ? event.touches[0].clientY : null;
    };

    const onTouchMove = (event: TouchEvent) => {
      if (lastTouchY === null || event.touches.length !== 1) return;
      const nextY = event.touches[0].clientY;
      scrollHistory(lastTouchY - nextY);
      lastTouchY = nextY;
      event.preventDefault();
    };

    const onTouchEnd = () => {
      lastTouchY = null;
    };

    container.addEventListener("wheel", onWheel, { capture: true, passive: false });
    container.addEventListener("touchstart", onTouchStart, { capture: true, passive: true });
    container.addEventListener("touchmove", onTouchMove, { capture: true, passive: false });
    container.addEventListener("touchend", onTouchEnd, { capture: true });
    container.addEventListener("touchcancel", onTouchEnd, { capture: true });

    return () => {
      container.removeEventListener("wheel", onWheel, { capture: true });
      container.removeEventListener("touchstart", onTouchStart, { capture: true });
      container.removeEventListener("touchmove", onTouchMove, { capture: true });
      container.removeEventListener("touchend", onTouchEnd, { capture: true });
      container.removeEventListener("touchcancel", onTouchEnd, { capture: true });
    };
  }, []);

  const startResizeDrag = (
    event: React.PointerEvent<HTMLButtonElement>,
    axis: "x" | "y",
  ) => {
    if (interactionMode !== "input") return;
    event.preventDefault();
    event.stopPropagation();
    onFocus();
    dragRef.current = {
      x: event.clientX,
      y: event.clientY,
      axis,
      pointerId: event.pointerId,
    };
    event.currentTarget.setPointerCapture(event.pointerId);
  };

  const finishResizeDrag = (event: React.PointerEvent<HTMLButtonElement>) => {
    const drag = dragRef.current;
    if (!drag || drag.pointerId !== event.pointerId) return;
    event.preventDefault();
    event.stopPropagation();
    dragRef.current = null;

    const container = containerRef.current;
    if (!container) return;
    const cols = Number(container.dataset.cols || pane.width || 80);
    const rows = Number(container.dataset.rows || pane.height || 24);
    const cellWidth =
      Number(container.dataset.cellWidth) || container.clientWidth / Math.max(cols, 1);
    const cellHeight =
      Number(container.dataset.cellHeight) || container.clientHeight / Math.max(rows, 1);

    if (drag.axis === "x") {
      const delta = event.clientX - drag.x;
      const amount = Math.abs(delta) / Math.max(cellWidth, 1);
      if (amount >= 1) {
        onResizePane(pane.id, delta > 0 ? "right" : "left", amount);
      }
    } else {
      const delta = event.clientY - drag.y;
      const amount = Math.abs(delta) / Math.max(cellHeight, 1);
      if (amount >= 1) {
        onResizePane(pane.id, delta > 0 ? "down" : "up", amount);
      }
    }
  };

  return (
    <div
      className={`desktopPane ${selected ? "selected" : ""}`}
      style={style}
      onMouseDown={onFocus}
    >
      <div className="desktopPaneLabel">{paneLabel(pane)}</div>
      <div ref={containerRef} className="desktopPaneSurface" />
      <button
        type="button"
        className="paneResizeHandle paneResizeHandleLeft"
        aria-label="Resize pane horizontally"
        onPointerDown={(event) => startResizeDrag(event, "x")}
        onPointerUp={finishResizeDrag}
        onPointerCancel={() => {
          dragRef.current = null;
        }}
      />
      <button
        type="button"
        className="paneResizeHandle paneResizeHandleRight"
        aria-label="Resize pane horizontally"
        onPointerDown={(event) => startResizeDrag(event, "x")}
        onPointerUp={finishResizeDrag}
        onPointerCancel={() => {
          dragRef.current = null;
        }}
      />
      <button
        type="button"
        className="paneResizeHandle paneResizeHandleTop"
        aria-label="Resize pane vertically"
        onPointerDown={(event) => startResizeDrag(event, "y")}
        onPointerUp={finishResizeDrag}
        onPointerCancel={() => {
          dragRef.current = null;
        }}
      />
      <button
        type="button"
        className="paneResizeHandle paneResizeHandleBottom"
        aria-label="Resize pane vertically"
        onPointerDown={(event) => startResizeDrag(event, "y")}
        onPointerUp={finishResizeDrag}
        onPointerCancel={() => {
          dragRef.current = null;
        }}
      />
    </div>
  );
}

function handlePaneTerminalTextMessage(data: string, term: Terminal): boolean {
  let message: ServerMessage;
  try {
    message = JSON.parse(data) as ServerMessage;
  } catch {
    term.write(data);
    return true;
  }

  if (message.type === "clear") {
    term.clear();
    return true;
  }
  return false;
}

function TmuxNavigator({
  state,
  focusedPane,
  onFocusPane,
  onCommand,
  onRenameWindow,
  onSetPaneNote,
}: {
  state: TmuxState;
  focusedPane: string | null;
  onFocusPane: (paneId: string) => void;
  onCommand: (command: TmuxCommand) => void;
  onRenameWindow: (windowId: string, current: string) => void;
  onSetPaneNote: (paneId: string, current: string) => void;
}) {
  const activePane =
    state.windows.flatMap((window) => window.panes).find((pane) => pane.id === focusedPane) ??
    state.windows.flatMap((window) => window.panes).find((pane) => pane.id === state.active_pane) ??
    null;

  return (
    <div className="tmuxNavigator">
      <div className="tmuxWindows">
        {state.windows.map((window) => (
          <span
            key={window.id}
            className={`tmuxChip ${window.panes.some((pane) => pane.id === activePane?.id) ? "selected" : ""}`}
          >
            <button type="button" onClick={() => window.panes[0] && onFocusPane(window.panes[0].id)}>
              {windowLabel(window)}
            </button>
            <button
              type="button"
              className="noteButton"
              title="Rename tab"
              onClick={() => onRenameWindow(window.id, window.name)}
            >
              <Pencil size={12} />
            </button>
          </span>
        ))}
      </div>
      <div className="tmuxPanes">
        {state.windows.flatMap((window) =>
          window.panes.map((pane) => (
            <span
              key={pane.id}
              className={`tmuxChip ${pane.id === activePane?.id ? "selected" : ""}`}
            >
              <button type="button" onClick={() => onFocusPane(pane.id)}>
                {paneLabel(pane)}
              </button>
              <button
                type="button"
                className="noteButton"
                title="Edit pane note"
                onClick={() => onSetPaneNote(pane.id, pane.note ?? "")}
              >
                <Pencil size={12} />
              </button>
            </span>
          )),
        )}
      </div>
      <div className="tmuxActions">
        <button type="button" onClick={() => onCommand("new_window")}>
          +Tab
        </button>
        <button type="button" onClick={() => onCommand("split_horizontal")} disabled={!activePane}>
          Split H
        </button>
        <button type="button" onClick={() => onCommand("split_vertical")} disabled={!activePane}>
          Split V
        </button>
      </div>
    </div>
  );
}

function getFallbackCellWidth(term: Terminal): number {
  return Number(term.options.fontSize ?? 14) * 0.62;
}

function getFallbackCellHeight(term: Terminal): number {
  const fontSize = Number(term.options.fontSize ?? 14);
  const lineHeight = Number(term.options.lineHeight ?? 1.15);
  return fontSize * lineHeight;
}

function clamp(value: number, min: number, max: number): number {
  return Math.max(min, Math.min(max, value));
}

function ModeSwitch({
  mode,
  onChange,
}: {
  mode: InteractionMode;
  onChange: (mode: InteractionMode) => void;
}) {
  return (
    <div className="modeSwitch" role="group" aria-label="Terminal mode">
      <button
        type="button"
        className={mode === "input" ? "selected" : ""}
        onClick={() => onChange("input")}
      >
        Input
      </button>
      <button
        type="button"
        className={mode === "browse" ? "selected" : ""}
        onClick={() => onChange("browse")}
      >
        Scroll
      </button>
    </div>
  );
}

function MobileKeybar({
  mode,
  onCopy,
  onSend,
}: {
  mode: InteractionMode;
  onCopy: () => void;
  onSend: (value: string) => void;
}) {
  const keys = [
    { label: "Esc", value: "\x1b" },
    { label: "Tab", value: "\t" },
    { label: "Enter", value: "\r" },
    { label: "Ctrl-C", value: "\x03" },
    { label: "cd ..", value: "cd ..\n" },
    { label: "↑", value: "\x1b[A" },
    { label: "↓", value: "\x1b[B" },
    { label: "←", value: "\x1b[D" },
    { label: "→", value: "\x1b[C" },
    { label: "|", value: "|" },
    { label: "/", value: "/" },
    { label: "-", value: "-" },
  ];

  return (
    <div className="mobileKeybar">
      {mode === "browse" ? (
        <button onClick={onCopy}>
          <Copy size={13} />
          Copy
        </button>
      ) : (
        keys.map((key) => (
          <button key={key.label} onClick={() => onSend(key.value)}>
            {key.label}
          </button>
        ))
      )}
    </div>
  );
}

function CreateSessionDialog({
  onClose,
  onCreate,
}: {
  onClose: () => void;
  onCreate: (payload: CreateSessionPayload) => Promise<void>;
}) {
  const [mode, setMode] = useState<SessionMode>("local_cc");
  const [name, setName] = useState("");
  const [tmuxName, setTmuxName] = useState("");
  const [host, setHost] = useState("");
  const [username, setUsername] = useState("");
  const [port, setPort] = useState("22");
  const [keyPath, setKeyPath] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const sshMode = mode === "ssh" || mode === "ssh_cc";

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (submitting) return;

    const payload: CreateSessionPayload = {
      name: name.trim() || undefined,
      mode,
      tmux_name: tmuxName.trim() || undefined,
      cols: 120,
      rows: 36,
    };

    if (sshMode) {
      payload.ssh = {
        host: host.trim(),
        username: username.trim(),
        port: Number(port || 22),
        key_path: keyPath.trim() || undefined,
      };
    }

    setSubmitting(true);
    setFormError(null);
    try {
      await onCreate(payload);
    } catch (err) {
      setFormError(err instanceof Error ? err.message : String(err));
      setSubmitting(false);
    }
  };

  return (
    <div className="modalBackdrop">
      <form className="dialog" onSubmit={submit}>
        <div className="dialogHeader">
          <h2>New session</h2>
          <button type="button" className="iconButton" onClick={onClose} disabled={submitting}>
            ×
          </button>
        </div>

        <label>
          Mode
          <div className="segmented">
            <button
              type="button"
              className={mode === "local_cc" ? "selected" : ""}
              disabled={submitting}
              onClick={() => setMode("local_cc")}
            >
              Local -CC
            </button>
            <button
              type="button"
              className={mode === "ssh_cc" ? "selected" : ""}
              disabled={submitting}
              onClick={() => setMode("ssh_cc")}
            >
              SSH -CC
            </button>
            <button
              type="button"
              className={mode === "local" ? "selected" : ""}
              disabled={submitting}
              onClick={() => setMode("local")}
            >
              Local legacy
            </button>
            <button
              type="button"
              className={mode === "ssh" ? "selected" : ""}
              disabled={submitting}
              onClick={() => setMode("ssh")}
            >
              SSH legacy
            </button>
          </div>
        </label>

        <label>
          Display name
          <input disabled={submitting} value={name} onChange={(event) => setName(event.target.value)} placeholder="Lab server" />
        </label>

        <label>
          tmux session
          <input disabled={submitting} value={tmuxName} onChange={(event) => setTmuxName(event.target.value)} placeholder="webterminal-main" />
        </label>

        {sshMode && (
          <div className="formGrid">
            <label>
              Host
              <input disabled={submitting} required value={host} onChange={(event) => setHost(event.target.value)} placeholder="192.168.10.20" />
            </label>
            <label>
              User
              <input disabled={submitting} required value={username} onChange={(event) => setUsername(event.target.value)} placeholder="root" />
            </label>
            <label>
              Port
              <input disabled={submitting} value={port} onChange={(event) => setPort(event.target.value)} inputMode="numeric" />
            </label>
            <label>
              Key path
              <input disabled={submitting} value={keyPath} onChange={(event) => setKeyPath(event.target.value)} placeholder="/Users/me/.ssh/id_ed25519" />
            </label>
          </div>
        )}

        {formError && (
          <DismissibleMessage className="formError" onClose={() => setFormError(null)}>
            {formError}
          </DismissibleMessage>
        )}

        <div className="dialogActions">
          <button type="button" className="secondaryButton" onClick={onClose} disabled={submitting}>
            Cancel
          </button>
          <button className="primaryButton compact" type="submit" disabled={submitting}>
            {submitting ? "Creating..." : "Create"}
          </button>
        </div>
      </form>
    </div>
  );
}

createRoot(document.getElementById("root")!).render(<App />);
