import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import net from "node:net";
import { setTimeout as sleep } from "node:timers/promises";

const ROOT = fileURLToPath(new URL("..", import.meta.url));
const EXTERNAL_BASE = process.env.WEBTERMINAL_TEST_BASE;
const TEST_TIMEOUT_MS = Number(process.env.WEBTERMINAL_TEST_TIMEOUT_MS ?? 15000);

const createdSessions = new Set();
let serverProcess = null;
let baseUrl = EXTERNAL_BASE;
let stoppingServer = false;

function log(message) {
  process.stdout.write(`${message}\n`);
}

function assert(condition, message) {
  if (!condition) {
    throw new Error(message);
  }
}

async function freePort() {
  const server = net.createServer();
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const address = server.address();
  await new Promise((resolve) => server.close(resolve));
  return address.port;
}

async function startServerIfNeeded() {
  if (baseUrl) {
    await waitForHealth(baseUrl);
    return;
  }

  const port = await freePort();
  baseUrl = `http://127.0.0.1:${port}`;
  const env = {
    ...process.env,
    WEBTERMINAL_ADDR: `127.0.0.1:${port}`,
    WEBTERMINAL_STATIC_DIR: `${ROOT}/frontend/dist`,
  };
  delete env.SSL_CERT_FILE;
  delete env.NODE_EXTRA_CA_CERTS;

  serverProcess = spawn("cargo", ["run", "--manifest-path", `${ROOT}/backend/Cargo.toml`], {
    cwd: ROOT,
    env,
    stdio: ["ignore", "pipe", "pipe"],
  });

  serverProcess.stdout.on("data", (chunk) => process.stdout.write(chunk));
  serverProcess.stderr.on("data", (chunk) => process.stderr.write(chunk));
  serverProcess.once("exit", (code, signal) => {
    if (stoppingServer) return;
    if (code !== null && code !== 0) {
      process.stderr.write(`backend exited with code ${code}\n`);
    }
    if (signal) {
      process.stderr.write(`backend exited with signal ${signal}\n`);
    }
  });

  await waitForHealth(baseUrl);
}

async function waitForHealth(url) {
  const started = Date.now();
  while (Date.now() - started < TEST_TIMEOUT_MS) {
    try {
      const res = await fetch(`${url}/api/health`);
      if (res.ok) return;
    } catch {
      // Retry until the server is ready.
    }
    await sleep(150);
  }
  throw new Error(`backend did not become healthy at ${url}`);
}

async function createSession(payload) {
  const res = await fetch(`${baseUrl}/api/sessions`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(payload),
  });
  if (!res.ok) {
    throw new Error(`create session failed: ${res.status} ${await res.text()}`);
  }
  const session = await res.json();
  createdSessions.add(session.id);
  return session;
}

async function deleteSession(id) {
  await fetch(`${baseUrl}/api/sessions/${id}`, { method: "DELETE" }).catch(() => {});
  createdSessions.delete(id);
}

function connect(sessionId, label = "client") {
  const wsUrl = `${baseUrl.replace(/^http/, "ws")}/ws/sessions/${sessionId}`;
  const ws = new WebSocket(wsUrl);
  const states = [];
  const focusEvents = [];
  const clearEvents = [];
  const fileDownloads = [];
  const fileStatuses = [];
  const chunks = [];
  ws.addEventListener("message", async (event) => {
    if (typeof event.data === "string") {
      try {
        const message = JSON.parse(event.data);
        if (message.type === "tmux_state") states.push(message.state);
        if (message.type === "focus_pane") focusEvents.push(message.pane_id);
        if (message.type === "clear") clearEvents.push(message);
        if (message.type === "file_download") fileDownloads.push(message);
        if (message.type === "file_transfer_status") fileStatuses.push(message);
      } catch {
        chunks.push(event.data);
      }
      return;
    }

    const bytes =
      event.data instanceof ArrayBuffer
        ? new Uint8Array(event.data)
        : new Uint8Array(await event.data.arrayBuffer());
    chunks.push(new TextDecoder().decode(bytes));
  });

  return { label, ws, states, focusEvents, clearEvents, fileDownloads, fileStatuses, chunks };
}

async function waitOpen(client) {
  if (client.ws.readyState === WebSocket.OPEN) return;
  await new Promise((resolve, reject) => {
    client.ws.addEventListener("open", resolve, { once: true });
    client.ws.addEventListener("error", reject, { once: true });
  });
}

async function closeClient(client) {
  if (client.ws.readyState === WebSocket.CLOSED) return;
  client.ws.close();
  await Promise.race([
    new Promise((resolve) => client.ws.addEventListener("close", resolve, { once: true })),
    sleep(1000),
  ]);
}

async function waitState(client, predicate, label, timeoutMs = TEST_TIMEOUT_MS) {
  const started = Date.now();
  while (Date.now() - started < timeoutMs) {
    const state = client.states.at(-1);
    if (state && predicate(state)) return state;
    await sleep(50);
  }
  throw new Error(
    `${client.label} timed out waiting for ${label}; last=${JSON.stringify(client.states.at(-1))}`,
  );
}

function panes(state) {
  return state.windows.flatMap((window) => window.panes);
}

function sendJson(client, message) {
  assert(client.ws.readyState === WebSocket.OPEN, `${client.label} websocket is not open`);
  client.ws.send(JSON.stringify(message));
}

async function waitFileDownload(client, id, timeoutMs = TEST_TIMEOUT_MS) {
  const started = Date.now();
  while (Date.now() - started < timeoutMs) {
    const found = client.fileDownloads?.find((message) => message.id === id);
    if (found) return found;
    const status = client.fileStatuses?.find((message) => message.id === id && message.status === "error");
    if (status) throw new Error(`${client.label} file transfer failed: ${status.message}`);
    await sleep(50);
  }
  throw new Error(
    `${client.label} timed out waiting for file download ${id}; statuses=${JSON.stringify(client.fileStatuses)} chunks=${client.chunks.join("")}`,
  );
}

async function waitForChunk(client, predicate, label, timeoutMs = TEST_TIMEOUT_MS) {
  const started = Date.now();
  while (Date.now() - started < timeoutMs) {
    if (client.chunks.some(predicate)) return;
    await sleep(50);
  }
  throw new Error(`${client.label} timed out waiting for ${label}; chunks=${client.chunks.join("")}`);
}

async function testDefaultModeAndMultiClientSync() {
  const session = await createSession({
    name: "it-default-sync",
    cols: 100,
    rows: 28,
    tmux_name: `wt_it_sync_${Date.now()}`,
  });
  assert(session.mode === "local_cc", `expected default local_cc, got ${session.mode}`);

  const a = connect(session.id, "client-a");
  const b = connect(session.id, "client-b");
  await Promise.all([waitOpen(a), waitOpen(b)]);

  let state = await waitState(a, (next) => panes(next).length === 1, "initial pane");
  await waitState(b, (next) => panes(next).length === 1, "initial pane on second client");

  const firstWindow = state.windows[0];
  const firstPane = panes(state)[0];
  sendJson(a, { type: "rename_window", window_id: firstWindow.id, name: "sync-tab-main" });
  await waitState(
    b,
    (next) => next.windows.some((window) => window.id === firstWindow.id && window.name === "sync-tab-main"),
    "renamed tab broadcast",
  );

  sendJson(b, { type: "set_pane_note", pane_id: firstPane.id, note: "sync-pane-main" });
  state = await waitState(
    a,
    (next) => panes(next).some((pane) => pane.id === firstPane.id && pane.note === "sync-pane-main"),
    "pane note broadcast",
  );

  sendJson(a, { type: "tmux_command", command: "split_horizontal" });
  state = await waitState(a, (next) => panes(next).length === 2, "split pane");
  const splitPane = panes(state).find((pane) => pane.id !== firstPane.id);
  assert(splitPane, "expected split pane to exist");

  sendJson(a, { type: "set_pane_note", pane_id: splitPane.id, note: "short-lived-pane" });
  await waitState(
    b,
    (next) => panes(next).some((pane) => pane.id === splitPane.id && pane.note === "short-lived-pane"),
    "split pane note broadcast",
  );

  await closeClient(a);
  const reconnected = connect(session.id, "client-reconnected");
  await waitOpen(reconnected);
  await waitState(
    reconnected,
    (next) =>
      next.windows.some((window) => window.name === "sync-tab-main") &&
      panes(next).some((pane) => pane.id === firstPane.id && pane.note === "sync-pane-main") &&
      panes(next).some((pane) => pane.id === splitPane.id && pane.note === "short-lived-pane"),
    "labels restored after reconnect",
  );

  sendJson(reconnected, { type: "focus_pane", pane_id: splitPane.id });
  await sleep(100);
  sendJson(reconnected, { type: "tmux_command", command: "kill_pane" });
  state = await waitState(
    b,
    (next) =>
      panes(next).every((pane) => pane.id !== splitPane.id) &&
      !JSON.stringify(next).includes("short-lived-pane"),
    "deleted pane note cleanup",
  );
  assert(panes(state).some((pane) => pane.note === "sync-pane-main"), "live pane note was lost");

  await closeClient(b);
  await closeClient(reconnected);
  await deleteSession(session.id);
}

async function testResizeKeepsControlModeAndLabels() {
  const session = await createSession({
    name: "it-resize-labels",
    mode: "local_cc",
    cols: 90,
    rows: 24,
    tmux_name: `wt_it_resize_${Date.now()}`,
  });
  const client = connect(session.id, "resize-client");
  await waitOpen(client);
  let state = await waitState(client, (next) => panes(next).length === 1, "initial pane");
  const pane = panes(state)[0];
  const window = state.windows[0];

  sendJson(client, { type: "rename_window", window_id: window.id, name: "resize-tab" });
  sendJson(client, { type: "set_pane_note", pane_id: pane.id, note: "resize-pane" });
  await waitState(
    client,
    (next) =>
      next.windows.some((item) => item.id === window.id && item.name === "resize-tab") &&
      panes(next).some((item) => item.id === pane.id && item.note === "resize-pane"),
    "labels before resize",
  );

  const res = await fetch(`${baseUrl}/api/sessions/${session.id}/resize`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ cols: 73, rows: 19 }),
  });
  assert(res.ok, `resize failed: ${res.status} ${await res.text()}`);

  await waitState(
    client,
    (next) =>
      next.windows.some((item) => item.name === "resize-tab") &&
      panes(next).some((item) => item.id === pane.id && item.note === "resize-pane"),
    "labels after resize",
  );

  sendJson(client, { type: "tmux_command", command: "new_window" });
  state = await waitState(client, (next) => next.windows.length >= 2, "new window after resize");
  assert(panes(state).length >= 2, "expected panes after new window");

  await closeClient(client);
  await deleteSession(session.id);
}

async function testMobileFixZoomDoesNotReplayLoop() {
  const session = await createSession({
    name: "it-mobile-fix-zoom",
    mode: "local_cc",
    cols: 82,
    rows: 28,
    tmux_name: `wt_it_zoom_${Date.now()}`,
  });
  const client = connect(session.id, "zoom-client");
  await waitOpen(client);
  let state = await waitState(client, (next) => panes(next).length === 1, "initial pane");
  const firstPane = panes(state)[0];

  sendJson(client, { type: "tmux_command", command: "split_horizontal", pane_id: firstPane.id });
  state = await waitState(client, (next) => panes(next).length === 2, "horizontal split");
  const splitPane = panes(state).find((pane) => pane.id !== firstPane.id);
  assert(splitPane, "expected split pane");
  sendJson(client, { type: "focus_pane", pane_id: firstPane.id });
  await sleep(150);

  const clearCountBefore = client.clearEvents.length;
  const chunkCountBefore = client.chunks.length;
  const resize = await fetch(`${baseUrl}/api/sessions/${session.id}/resize`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ cols: 82, rows: 30, zoom_pane_id: firstPane.id }),
  });
  assert(resize.ok, `zoom resize failed: ${resize.status} ${await resize.text()}`);

  state = await waitState(
    client,
    (next) => {
      const activeWindow = next.windows.find((window) =>
        window.panes.some((pane) => pane.id === firstPane.id),
      );
      const activePane = panes(next).find((pane) => pane.id === firstPane.id);
      return activeWindow?.zoomed && activePane?.width === 82 && activePane?.height === 30;
    },
    "zoomed pane after mobile fix resize",
  );
  const activeWindow = state.windows.find((window) => window.panes.some((pane) => pane.id === firstPane.id));
  const activePane = panes(state).find((pane) => pane.id === firstPane.id);
  assert(activeWindow?.zoomed, "window should be zoomed after mobile fix");
  assert(activePane?.width === 82, `zoomed pane width mismatch: ${activePane?.width}`);
  assert(activePane?.height === 30, `zoomed pane height mismatch: ${activePane?.height}`);

  await sleep(1000);
  assert(
    client.clearEvents.length === clearCountBefore,
    `resize broadcast ${client.clearEvents.length - clearCountBefore} clear events`,
  );
  const resizeOutputBytes = client.chunks
    .slice(chunkCountBefore)
    .reduce((total, chunk) => total + chunk.length, 0);
  assert(
    resizeOutputBytes < 32 * 1024,
    `resize replayed too much output: ${resizeOutputBytes} bytes`,
  );

  await closeClient(client);
  await deleteSession(session.id);
}

async function testTerminalFileBridge() {
  const session = await createSession({
    name: "it-file-bridge",
    mode: "local_cc",
    cols: 90,
    rows: 24,
    tmux_name: `wt_it_files_${Date.now()}`,
  });
  const client = connect(session.id, "file-client");
  await waitOpen(client);
  const state = await waitState(client, (next) => panes(next).length === 1, "initial pane");
  const pane = panes(state)[0];
  const path = `/tmp/webterminal-it-${Date.now()}.txt`;
  const text = "hello from webterminal file bridge\nsecond line\n";
  const uploadId = `upload_${Date.now()}`;
  const downloadId = `download_${Date.now()}`;
  const data = Buffer.from(text).toString("base64");

  sendJson(client, { type: "file_upload_start", id: uploadId, path, pane_id: pane.id });
  sendJson(client, { type: "file_upload_chunk", id: uploadId, data });
  sendJson(client, { type: "file_upload_finish", id: uploadId });
  await waitForChunk(client, (chunk) => chunk.includes("[webterminal upload complete:"), "upload complete");

  sendJson(client, { type: "file_download", id: downloadId, path, pane_id: pane.id });
  const download = await waitFileDownload(client, downloadId);
  const roundTrip = Buffer.from(download.data_base64, "base64").toString("utf8");
  assert(roundTrip === text, `download mismatch: ${JSON.stringify(roundTrip)}`);
  const filename = Buffer.from(download.filename_base64, "base64").toString("utf8");
  assert(filename === path.split("/").at(-1), `unexpected filename: ${filename}`);

  sendJson(client, {
    type: "tmux_command",
    command: "kill_pane",
    pane_id: pane.id,
  });
  await closeClient(client);
  await deleteSession(session.id);
}

async function cleanup() {
  for (const sessionId of [...createdSessions]) {
    await deleteSession(sessionId);
  }
  if (serverProcess) {
    stoppingServer = true;
    serverProcess.kill("SIGTERM");
    await Promise.race([
      new Promise((resolve) => serverProcess.once("exit", resolve)),
      sleep(2000).then(() => serverProcess.kill("SIGKILL")),
    ]);
  }
}

async function run() {
  await startServerIfNeeded();
  log(`Testing ${baseUrl}`);

  await testDefaultModeAndMultiClientSync();
  log("ok default mode, multi-client sync, reconnect, pane cleanup");

  await testResizeKeepsControlModeAndLabels();
  log("ok resize preserves tmux control labels and commands");

  await testMobileFixZoomDoesNotReplayLoop();
  log("ok mobile fix zooms pane without replay loop");

  await testTerminalFileBridge();
  log("ok terminal file bridge upload/download");
}

run()
  .then(async () => {
    await cleanup();
    log("PASS tmux control integration");
  })
  .catch(async (err) => {
    await cleanup();
    console.error(err);
    process.exit(1);
  });
