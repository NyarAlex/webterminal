# WebTerminal

A self-hosted persistent terminal hub. V1 keeps terminal sessions on the
server side, lets multiple browsers watch and type into the same terminal, and
uses tmux as the persistence layer.

## V1 Scope

- Rust backend with Axum and WebSocket.
- Server-owned PTY sessions using `portable-pty`.
- Local tmux sessions with replay buffer and multi-viewer broadcast.
- SSH tmux sessions through system OpenSSH:
  `ssh -tt user@host 'tmux new-session -A -s <name>'`.
- React + xterm.js frontend with a mobile shortcut bar.
- Canonical terminal size controlled by the server, not by every browser.

The first version intentionally avoids a device agent, full `tmux -CC`, and a
multi-user permission model. Those are later layers.

## Requirements

- Rust 1.88+
- Node.js 24+
- tmux
- OpenSSH client

## Run

Install frontend dependencies:

```bash
cd frontend
env -u SSL_CERT_FILE -u NODE_EXTRA_CA_CERTS npm install
```

Build the frontend:

```bash
cd frontend
env -u SSL_CERT_FILE -u NODE_EXTRA_CA_CERTS npm run build
```

Run the backend from `backend/`:

```bash
cd backend
WEBTERMINAL_STATIC_DIR=../frontend/dist WEBTERMINAL_DATA_DIR=../data env -u SSL_CERT_FILE cargo run
```

Open:

```text
http://127.0.0.1:8787
```

## Development

Run the backend:

```bash
cd backend
env -u SSL_CERT_FILE cargo run
```

Run the frontend dev server:

```bash
cd frontend
env -u SSL_CERT_FILE -u NODE_EXTRA_CA_CERTS npm run dev
```

Open:

```text
http://127.0.0.1:5173
```

The Vite dev server proxies `/api` and `/ws` to the Rust backend.

## Persistence

WebTerminal stores recoverable tmux session metadata in
`$WEBTERMINAL_DATA_DIR/sessions.json` (`../data` by default when running from
`backend/`). Persist this directory in Docker or systemd deployments.

On backend startup, saved `local`, `local_cc`, `ssh`, and `ssh_cc` sessions are
reattached with `tmux new-session -A` or `tmux -CC new-session -A` using the
saved tmux session name and SSH target. Deleting a session from the UI removes
it from the store; restarting the backend does not remove the target tmux
session.

Custom one-off commands are not restored because they do not have stable tmux
attach semantics.

## Tests

Run the tmux control-mode integration tests:

```bash
env -u SSL_CERT_FILE -u NODE_EXTRA_CA_CERTS npm run test:integration
```

The test starts a temporary backend on a free local port, creates real tmux
control-mode sessions, verifies multi-browser state sync, reconnect restore,
resize behavior, and stale pane-note cleanup after pane deletion.

To test an already running backend:

```bash
WEBTERMINAL_TEST_BASE=http://127.0.0.1:8787 env -u SSL_CERT_FILE -u NODE_EXTRA_CA_CERTS npm run test:integration
```

## Session Model

Each session has one canonical PTY on the Linux/Mac hub. Browser clients are
viewers/controllers of that session:

- Output from PTY is broadcast to all connected browsers.
- Input from any browser is written to the same PTY.
- New browsers receive a replay of recent output before live output.
- Closing the browser does not kill the PTY.
- Deleting a session from the UI terminates the PTY process.

## Next Modules

- SQLite device store.
- Encrypted private key storage using a local master key file.
- Known-hosts review and trust-on-first-use UX.
- Explicit Detach vs Terminate actions.
- Linux systemd deployment.
