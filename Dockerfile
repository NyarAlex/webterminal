FROM node:24-bookworm AS frontend
WORKDIR /app/frontend
COPY frontend/package*.json frontend/.npmrc ./
RUN env -u SSL_CERT_FILE -u NODE_EXTRA_CA_CERTS npm install
COPY frontend ./
RUN env -u SSL_CERT_FILE -u NODE_EXTRA_CA_CERTS npm run build

FROM rust:1.88-bookworm AS backend
WORKDIR /app/backend
COPY backend/Cargo.toml backend/Cargo.lock ./
COPY backend/src ./src
RUN env -u SSL_CERT_FILE cargo build --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates openssh-client tmux \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=backend /app/backend/target/release/backend /app/webterminal
COPY --from=frontend /app/frontend/dist /app/frontend/dist
ENV WEBTERMINAL_ADDR=0.0.0.0:8787
ENV WEBTERMINAL_STATIC_DIR=/app/frontend/dist
EXPOSE 8787
CMD ["/app/webterminal"]
