# Deploying memstroy-assets-server to Railway

The shared assets server is an axum-based HTTP service that serves
clips / videos / images / sounds / particles / text resources to Memstroy
editors. Production is designed for Railway Buckets (S3-compatible object
storage): the server owns catalogue/search/admin upload, while previews and
downloads redirect to short-lived presigned bucket URLs so video traffic does
not pass through the Railway web service.

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
2. **Add a Railway Bucket:**
   - Create → Bucket
   - Open the bucket → Credentials
   - Click **Add to Service** for the `web` service, or manually add the
     AWS-style variables shown by Railway:
     `AWS_ENDPOINT_URL`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
     `AWS_S3_BUCKET_NAME`, `AWS_DEFAULT_REGION`, `AWS_S3_URL_STYLE`.
3. **Keep a small local working directory:**
   - A Volume is no longer the serving store, but it is still useful as
     temporary space while uploads are processed.
   - If you plan to rerun the parser, no Volume-to-Bucket migration is needed:
     the bucket can start empty and the parser will repopulate it through
     `POST /api/admin/assets`.
4. **Set environment variables** (Settings → Variables):
   - `RUST_LOG` = `info,memstroy_assets_server=info`
   - `MEMSTROY_STORAGE_BACKEND` = `bucket`
   - `MEMSTROY_BUCKET_PRESIGN_SECS` = `3600`
   - `MEMSTROY_ADMIN_TOKEN` (or legacy `ADMIN_TOKEN`) protects
     `POST /api/admin/assets`.
5. Railway will build via the `nixpacks.toml` config (in repo root):
   - Build: `cargo build --release -p memstroy-assets-server`
   - Start: `./target/release/memstroy-assets-server`
   - **Note**: Only the server package is built on Railway. The `-p memstroy-assets-server`
     flag is required because the workspace's `default-members` excludes the server package
     to speed up local client builds.
6. **Configure a public domain** (Settings → Networking → Generate domain)
   so the editor clients can reach the server.

Railway Buckets are private and public buckets are not supported. The server
therefore returns `307 Temporary Redirect` from preview/download endpoints to
presigned URLs. This is intentional: bucket egress is free on Railway, while
service egress is not.

## Rebuilding the bucket from the parser

For a clean bucket-backed rebuild:

1. Deploy the server with `MEMSTROY_STORAGE_BACKEND=bucket`.
2. Confirm `/api/health` returns `storage_backend: "bucket"` and
   `bucket_configured: true`. `count` can be `0` before the parser runs.
3. Run the Telegram parser again against the same public API URL.
4. If the parser already has `state.json` entries marked as uploaded, either
   delete/rename that parser state file or run it with
   `--force --reupload-existing` so it uploads everything again.

The parser does not need bucket credentials. It keeps using
`POST /api/admin/assets`; the server stores those uploads in the Railway
Bucket.

## Endpoint summary

- `GET /api/health` — health check plus storage diagnostics
  (`storage_backend`, `bucket_configured`, `bucket_name`,
  `bucket_endpoint`, `bucket_presign_secs`) and asset-root diagnostics
  (`asset_root`, `railway_volume_mount_path`, `root_inside_railway_volume`,
  `asset_root_writable`, counts by kind)
- `GET /api/assets?kind=clip&limit=100&offset=0&q=query` — list assets with paginated fuzzy search
- `GET /api/assets/:id` — full asset record (path, mime, etc.)
- `GET /api/assets/:id/preview` — local thumbnail bytes in local mode, or
  `307` to a presigned bucket URL in bucket mode
- `GET /api/assets/:id/download` — local bytes in local mode, or `307` to a
  presigned bucket URL in bucket mode
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

If `MEMSTROY_ADMIN_TOKEN` or `ADMIN_TOKEN` is set in Railway variables,
calls must include either `Authorization: Bearer <token>` or
`X-Admin-Token: <token>`. In bucket mode uploads are accepted by the server,
processed locally for thumbnails/metadata, stored in the Railway Bucket, and
then indexed immediately.

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

The server should not be the video CDN:

- The in-memory index stores metadata only.
- `GET /api/assets` and `GET /api/assets/:id` are small JSON responses.
- Preview/download endpoints return presigned redirects in bucket mode.
- Bucket egress is handled by Railway Bucket, not the web service.
- Keep web replicas focused on API/admin traffic; scale replicas if catalogue
  JSON/API request volume grows.

## Local testing of the deploy build

```bash
cargo build --release -p memstroy-assets-server
PORT=8080 ASSETS_ROOT=./assets ./target/release/memstroy-assets-server
curl http://localhost:8080/api/health
```

To test bucket mode locally, provide the same S3 variables Railway shows in
the bucket Credentials tab and run:

```bash
MEMSTROY_STORAGE_BACKEND=bucket PORT=8080 ASSETS_ROOT=./assets \
  ./target/release/memstroy-assets-server
```

## Volume troubleshooting

On Railway, the server logs `persistent asset volume ready` with the resolved
root path. You can also check it over HTTP:

```bash
curl https://your-app.up.railway.app/api/health
```

For bucket mode, `/api/health` must show:

- `storage_backend: "bucket"`
- `bucket_configured: true`
- `count` equal to the number of indexed objects

Check redirect behavior without downloading the whole video:

```bash
curl -I https://your-app.up.railway.app/api/assets/tg_123/download
```

Expected result: `307 Temporary Redirect` with a `Location:` URL pointing at
the Railway Bucket endpoint.

If `Volume Usage` stays at `0 B` in bucket mode, that is fine: the bucket, not
the Volume, is the persistent serving store. If the bucket file list is empty
after parser upload, confirm the bucket credentials are added to the web
service and check `/api/health` for `storage_backend: "bucket"`.

`MEMSTROY_BUCKET_MIGRATE_LOCAL=1` still exists as a one-time legacy migration
switch, but it is not part of the normal flow when the parser will be rerun.
