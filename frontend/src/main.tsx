import React, { useCallback, useEffect, useMemo, useState } from "react";
import { createRoot } from "react-dom/client";
import {
  ChevronRight,
  Circle,
  Command,
  Copy,
  Keyboard,
  Maximize2,
  Menu,
  Monitor,
  Plus,
  RefreshCw,
  Server,
  Smartphone,
  Trash2,
  Wifi,
  X,
} from "lucide-react";
import { Terminal } from "@xterm/xterm";
import { WebLinksAddon } from "@xterm/addon-web-links";
import "@xterm/xterm/css/xterm.css";
import "./styles.css";

type SessionMode = "local" | "ssh" | "local_cc" | "ssh_cc";
type InteractionMode = "input" | "browse";

interface SessionSummary {
  id: string;
  name: string;
  command: string;
  mode: SessionMode;
  cols: number;
  rows: number;
  created_at: string;
  viewers: number;
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

function App() {
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [formOpen, setFormOpen] = useState(false);
  const [drawerOpen, setDrawerOpen] = useState(false);
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
    setActiveId((current) => current ?? next[0]?.id ?? null);
  };

  useEffect(() => {
    void refresh().catch((err: unknown) => setError(String(err)));
  }, []);

  const createSession = async (payload: CreateSessionPayload) => {
    setError(null);
    const session = await api.createSession(payload);
    await refresh();
    setActiveId(session.id);
    setFormOpen(false);
    setDrawerOpen(false);
  };

  const deleteSession = async (id: string) => {
    await api.deleteSession(id);
    const next = sessions.filter((session) => session.id !== id);
    setSessions(next);
    if (activeId === id) setActiveId(next[0]?.id ?? null);
  };

  const updateSession = useCallback((nextSession: SessionSummary) => {
    setSessions((current) =>
      current.map((session) => (session.id === nextSession.id ? nextSession : session)),
    );
  }, []);

  return (
    <main className="appShell">
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
        </div>

        <button className="primaryButton" onClick={() => setFormOpen(true)}>
          <Plus size={17} />
          New session
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
              <button
                key={session.id}
                className={`sessionItem ${session.id === activeId ? "active" : ""}`}
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
                    {session.cols}x{session.rows} · {session.viewers} viewers
                  </small>
                </span>
                <ChevronRight size={15} />
              </button>
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
            <Circle className={active ? "liveDot" : "idleDot"} size={10} fill="currentColor" />
            <span>{active ? active.name : "No active terminal"}</span>
          </div>
          <div className="topbarActions">
            {active && (
              <>
                <span className="statusPill">{active.mode}</span>
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
          </div>
        </header>

        {error && <div className="errorBanner">{error}</div>}

        {active ? (
          <TerminalPane
            key={active.id}
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
            <p>Start locally with tmux, then connect the same session from phone and desktop.</p>
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
        {formError && <div className="formError">{formError}</div>}

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
  const termRef = React.useRef<Terminal | null>(null);
  const wsRef = React.useRef<WebSocket | null>(null);
  const inputEnabledRef = React.useRef(true);
  const lastModeRef = React.useRef<InteractionMode>("input");
  const [connected, setConnected] = useState(false);
  const [interactionMode, setInteractionMode] = useState<InteractionMode>("input");
  const controlMode = session.mode === "local_cc" || session.mode === "ssh_cc";

  useEffect(() => {
    const term = new Terminal({
      cols: session.cols,
      rows: session.rows,
      cursorBlink: true,
      convertEol: false,
      fontFamily:
        "SFMono-Regular, ui-monospace, Menlo, Monaco, Consolas, Liberation Mono, monospace",
      fontSize: window.innerWidth < 720 ? 12 : 14,
      lineHeight: 1.15,
      scrollback: 5000,
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
    termRef.current = term;

    if (!containerRef.current) return;
    term.open(containerRef.current);

    const proto = window.location.protocol === "https:" ? "wss" : "ws";
    const ws = new WebSocket(`${proto}://${window.location.host}/ws/sessions/${session.id}`);
    ws.binaryType = "arraybuffer";
    wsRef.current = ws;

    ws.onopen = () => {
      setConnected(true);
      term.focus();
    };
    ws.onclose = () => setConnected(false);
    ws.onerror = () => setConnected(false);
    ws.onmessage = (event) => {
      if (event.data instanceof ArrayBuffer) {
        term.write(new Uint8Array(event.data));
      } else if (event.data instanceof Blob) {
        void event.data.arrayBuffer().then((buffer) => term.write(new Uint8Array(buffer)));
      } else {
        term.write(String(event.data));
      }
    };

    const dataSub = term.onData((data) => {
      if (inputEnabledRef.current && ws.readyState === WebSocket.OPEN) ws.send(data);
    });

    return () => {
      dataSub.dispose();
      ws.close();
      term.dispose();
      termRef.current = null;
      wsRef.current = null;
    };
  }, [session.id, session.cols, session.rows]);

  useEffect(() => {
    inputEnabledRef.current = interactionMode === "input";
    const term = termRef.current;
    const ws = wsRef.current;
    if (!term) return;

    term.options.disableStdin = interactionMode === "browse";
    if (interactionMode === "input") {
      if (!controlMode && lastModeRef.current === "browse" && ws?.readyState === WebSocket.OPEN) {
        ws.send("\x1b");
      }
      term.scrollToBottom();
      term.focus();
    } else {
      if (!controlMode && lastModeRef.current === "input" && ws?.readyState === WebSocket.OPEN) {
        ws.send("\x02[");
      }
      term.blur();
    }
    lastModeRef.current = interactionMode;
  }, [controlMode, interactionMode]);

  useEffect(() => {
    if (controlMode || interactionMode !== "browse") return;

    const container = containerRef.current;
    const viewport = container?.querySelector<HTMLElement>(".xterm-viewport");
    if (!container || !viewport) return;

    let lastTouchY: number | null = null;
    let wheelRemainder = 0;
    const lineThreshold = 12;

    const sendTmuxScroll = (deltaY: number) => {
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
      viewport.scrollTop += deltaY;
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
  }, [controlMode, interactionMode]);

  useEffect(() => {
    if (resizeToken === 0) return;

    const term = termRef.current;
    const container = containerRef.current;
    if (!term || !container) return;

    const nextSize = measureTerminalGrid(container, term);
    term.resize(nextSize.cols, nextSize.rows);
    term.scrollToBottom();

    void api
      .resizeSession(session.id, nextSize)
      .then(onResized)
      .catch((err: unknown) => {
        onError(err instanceof Error ? err.message : String(err));
      });
  }, [resizeToken, session.id, onResized, onError]);

  const send = (value: string) => {
    if (interactionMode !== "input") return;
    const ws = wsRef.current;
    if (ws?.readyState === WebSocket.OPEN) ws.send(value);
  };

  const copySelection = async () => {
    const selection = termRef.current?.getSelection() ?? "";
    if (!selection) return;
    await navigator.clipboard.writeText(selection);
  };

  return (
    <div className="terminalWrap">
      <div className="terminalChrome">
        <span className={connected ? "connected" : "disconnected"}>
          {connected ? "connected" : "disconnected"}
        </span>
        <span className="terminalCommand">{session.command}</span>
        <ModeSwitch mode={interactionMode} onChange={setInteractionMode} />
        <button className="chromeButton" type="button" onClick={() => void copySelection()}>
          <Copy size={14} />
          Copy
        </button>
        <Maximize2 size={15} />
      </div>
      <div
        ref={containerRef}
        className={`terminalSurface ${interactionMode === "browse" ? "browseMode" : "inputMode"}`}
      />
      <MobileKeybar
        mode={interactionMode}
        onModeChange={setInteractionMode}
        onCopy={() => void copySelection()}
        onSend={send}
      />
    </div>
  );
}

function measureTerminalGrid(container: HTMLElement, term: Terminal): ResizeSessionPayload {
  const styles = window.getComputedStyle(container);
  const width =
    container.clientWidth - parseFloat(styles.paddingLeft) - parseFloat(styles.paddingRight);
  const height =
    container.clientHeight - parseFloat(styles.paddingTop) - parseFloat(styles.paddingBottom);
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
  onModeChange,
  onCopy,
  onSend,
}: {
  mode: InteractionMode;
  onModeChange: (mode: InteractionMode) => void;
  onCopy: () => void;
  onSend: (value: string) => void;
}) {
  const keys = [
    { label: "Esc", value: "\x1b" },
    { label: "Tab", value: "\t" },
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
      <Keyboard size={16} />
      <button
        className={mode === "input" ? "selected" : ""}
        onClick={() => onModeChange("input")}
      >
        Input
      </button>
      <button
        className={mode === "browse" ? "selected" : ""}
        onClick={() => onModeChange("browse")}
      >
        Scroll
      </button>
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
  const [mode, setMode] = useState<SessionMode>("local");
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
              className={mode === "local" ? "selected" : ""}
              disabled={submitting}
              onClick={() => setMode("local")}
            >
              Local tmux
            </button>
            <button
              type="button"
              className={mode === "ssh" ? "selected" : ""}
              disabled={submitting}
              onClick={() => setMode("ssh")}
            >
              SSH tmux
            </button>
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

        {formError && <div className="formError">{formError}</div>}

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
