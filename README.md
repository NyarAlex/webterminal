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
WEBTERMINAL_STATIC_DIR=../frontend/dist env -u SSL_CERT_FILE cargo run
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
