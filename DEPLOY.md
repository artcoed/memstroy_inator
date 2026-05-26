# Deploying memstroy-assets-server to Railway

The shared assets server is a small axum-based HTTP service that serves
clips / images / sounds / particles to any number of Memstroy editors.
It's designed to run on a single host and stream assets on demand.

## Quick start (Railway)

1. **Create a new Railway project** linked to this repository.
2. **Add a persistent Volume:**
   - Settings → Volumes → New Volume
   - Mount path: `/data`
   - Size: 5–20 GB depending on how many clips you plan to scrape
3. **Set environment variables** (Settings → Variables):
   - `RUST_LOG` = `info,memstroy_assets_server=info`
   - `ASSETS_ROOT` = `/data/assets` (already in railway.toml as default)
4. Railway will build via the `nixpacks.toml` config:
   - `cargo build --release --bin memstroy-assets-server`
   - Start: `./target/release/memstroy-assets-server --root /data/assets`
5. **Configure a public domain** (Settings → Networking → Generate domain)
   so the editor clients can reach the server.

## Endpoint summary

- `GET /api/health` — health check (used by Railway's healthcheck)
- `GET /api/assets?kind=clip&limit=100&offset=0` — list assets (paginated)
- `GET /api/assets/:id` — full asset record (path, mime, etc.)
- `GET /api/assets/:id/preview` — thumbnail bytes (if available)
- `GET /api/assets/:id/download` — full asset bytes (lazy download)
- `GET /api/assets/:id/text` — text sidecar bytes
- `POST /api/ingest/tg` — kick a Telegram channel ingest (body: `{"channel": "name", "limit": 500}`)

## Editor client configuration

In the editor, set the server URL via `MEMSTROY_DEFAULT_SERVER_URL` env
var at compile time (baked into the binary), or runtime by editing the
`server_url` field in the saved app state.

For a Railway deployment that's reachable at `https://my-app.up.railway.app`,
build the editor with:

```bash
MEMSTROY_DEFAULT_SERVER_URL=https://my-app.up.railway.app cargo build --release -p memstroy-gui
```

## Resource sizing

The server is mostly disk-bound:
- ~50 MB RAM at idle
- 1 GB tier is enough for development; bump to 2 GB if you ingest
  large channels (>1000 clips) frequently
- Disk: each clip is ~5–20 MB; budget accordingly on the volume

## Local testing of the deploy build

```bash
cargo build --release --bin memstroy-assets-server
PORT=8080 ASSETS_ROOT=./assets ./target/release/memstroy-assets-server
curl http://localhost:8080/api/health
```
