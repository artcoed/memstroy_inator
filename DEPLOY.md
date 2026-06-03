# Deploying memstroy-assets-server to Railway

The shared assets server is an axum-based HTTP service that serves
clips / videos / images / sounds / particles / text resources to Memstroy
editors. It is designed for Railway with a persistent Volume: admins upload
resources into the mounted volume, the server re-indexes immediately, and users
can search and stream assets on demand.

## Build configuration

The project uses **conditional compilation** to optimize build times:

- **Local development** (`cargo build --release`): Builds GUI with embedded
  local assets-server for convenience. This includes the full dependency tree.
  
- **Client distribution** (`scripts/package-client.ps1`): Builds GUI **without**
  the assets-server dependency using `--no-default-features`. This excludes
  heavy dependencies like `axum` and `tower-http`, cutting build
  time roughly in half and reducing binary size.
  
- **Railway deployment**: Only builds `memstroy-assets-server` binary via
  `nixpacks.toml`, not the entire workspace.

## Quick start (Railway)

1. **Create a new Railway project** linked to this repository.
2. **Add a persistent Volume:**
   - Settings → Volumes → New Volume
   - Mount path: any absolute path, for example `/assets` or `/data`
   - Size: 20+ GB depending on the expected video library size
3. **Set environment variables** (Settings → Variables):
   - `RUST_LOG` = `info,memstroy_assets_server=info`
   - `ASSETS_ROOT` is optional. If set, it must point inside the mounted
     Railway Volume. Otherwise the server uses Railway's automatic
     `RAILWAY_VOLUME_MOUNT_PATH`.
4. Railway will build via the `nixpacks.toml` config (in repo root):
   - Build: `cargo build --release -p memstroy-assets-server`
   - Start: `./target/release/memstroy-assets-server`
   - **Note**: Only the server package is built on Railway. The `-p memstroy-assets-server`
     flag is required because the workspace's `default-members` excludes the server package
     to speed up local client builds.
5. **Configure a public domain** (Settings → Networking → Generate domain)
   so the editor clients can reach the server.

## Endpoint summary

- `GET /api/health` — health check (used by Railway's healthcheck)
- `GET /api/assets?kind=clip&limit=100&offset=0&q=query` — list assets with paginated fuzzy search
- `GET /api/assets/:id` — full asset record (path, mime, etc.)
- `GET /api/assets/:id/preview` — thumbnail bytes (if available)
- `GET /api/assets/:id/download` — full asset bytes (lazy download)
- `GET /api/assets/:id/text` — text sidecar bytes
- `POST /api/admin/assets` — admin resource upload via `multipart/form-data`

## Admin upload contract

`POST /api/admin/assets` accepts:

- `kind`: one of `clip`, `video`, `image`, `sound`, `particle`, `text`
- `asset`: primary file. For clips/videos this is the video file.
- `description`: free-form text sidecar. For clips this is the clip description.
- `id`: optional stable id. If omitted, the server derives it from the filename.
- `label`: optional display name.
- `tags`: optional comma- or newline-separated tags.
- `thumbnail`: optional `png`, `jpg`, `jpeg`, or `webp` preview.

If `ADMIN_TOKEN` is set in Railway variables, calls must include either
`Authorization: Bearer <token>` or `X-Admin-Token: <token>`.

Example:

```bash
curl -X POST https://your-app.up.railway.app/api/admin/assets \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -F kind=clip \
  -F description="Funny clip description" \
  -F tags="meme,short" \
  -F asset=@clip.mp4 \
  -F thumbnail=@clip.jpg
```

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
- Assets are streamed from disk; downloads do not buffer entire videos in RAM.
- The in-memory index stores metadata only.
- Disk: each clip is commonly 5–50 MB; size the Railway Volume for the catalogue.

## Local testing of the deploy build

```bash
cargo build --release -p memstroy-assets-server
PORT=8080 ASSETS_ROOT=./assets ./target/release/memstroy-assets-server
curl http://localhost:8080/api/health
```

## Volume troubleshooting

On Railway, the server logs `persistent asset volume ready` with the resolved
root path. If Volume Usage stays at `0 B` while uploads return success, the
server is probably writing outside the mounted volume. Check the Volume
Settings mount path and either remove `ASSETS_ROOT` or set it to that mount
path (or a subdirectory inside it).
