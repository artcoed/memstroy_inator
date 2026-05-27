# Build Guide

## Overview

The project uses **conditional compilation** to optimize build times for different scenarios:

- **Local development**: Full feature set including embedded assets-server
- **Client distribution**: Minimal build without server dependencies
- **Railway deployment**: Server-only build

## Local Development

Build everything with default features (includes local assets-server):

```powershell
cargo build --release
```

Or run the GUI directly:

```powershell
cargo run -p memstroy-gui
```

The GUI will automatically start a local assets-server on `http://127.0.0.1:8080`.

## Client Distribution (Windows Installer)

Create an installer for distribution to clients:

```powershell
pwsh scripts/make-installer.ps1 -ServerUrl "https://your-server.railway.app" -AllowLoopback
```

This internally calls `scripts/package-client.ps1` which:
- Builds GUI **without** `local-server` feature (`--no-default-features`)
- Excludes heavy dependencies: `axum`, `tower-http`, `scraper`, etc.
- Reduces build time by ~50%
- Reduces binary size significantly

The resulting installer will connect to your remote assets-server only.

## Railway Deployment (Server Only)

Railway automatically builds only the server binary via `nixpacks.toml`:

```bash
cargo build --release --bin memstroy-assets-server
```

This builds **only** the server, not the entire workspace.

## Build Time Comparison

| Scenario | Command | Build Time | Dependencies |
|----------|---------|------------|--------------|
| **Local dev** | `cargo build --release` | ~Full | All (GUI + server) |
| **Client** | `scripts/package-client.ps1` | ~50% faster | GUI only (no server) |
| **Railway** | `nixpacks.toml` | Minimal | Server only |

## Why Two Binaries?

The project produces two separate executables:

1. **`memstroy-gui.exe`** / **`memstroy.exe`** (CLI)
   - Distributed to clients via installer
   - Connects to remote assets-server
   - Built **without** server dependencies in client mode

2. **`memstroy-assets-server.exe`**
   - Runs on Railway (or your server)
   - Serves assets to all clients
   - **Not** included in client installer

## Troubleshooting

### "Feature local-server not found"

Make sure you're using the correct build command:
- Local dev: `cargo build` (uses default features)
- Client: Use `scripts/package-client.ps1` (disables local-server)

### "Server not starting locally"

Check that `local-server` feature is enabled (it's in default features).
If you manually disabled it, re-enable:

```powershell
cargo build -p memstroy-gui --features local-server
```

### "Build time still slow"

Make sure you're using the packaging script for client builds:

```powershell
# ❌ Slow (builds everything)
cargo build --release

# ✅ Fast (client mode, no server)
pwsh scripts/package-client.ps1 -ServerUrl "https://your-server.railway.app"
```
