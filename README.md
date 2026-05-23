# memstroy-inator

> Formerly known as `memstroy_generator`. The workspace directory and
> per-crate package names (`memstroy-core`, `memstroy-gui`, …) are kept
> for compatibility with existing scripts and project files; the
> product is now branded **memstroy-inator** in the editor window
> title, the README and the Cargo descriptions.

A desktop tool for assembling **Mellstroy-style memes** for short
vertical videos. The workflow is:

1. **Download** raw Mellstroy clips from the public Telegram channel
   [`@MELLSTROYfonz`](https://t.me/MELLSTROYfonz). Only posts whose
   body text contains "Имба" are kept by default — the channel's own
   convention for share-worthy clips.
2. **Detect anchors** on each downloaded clip with a pose model so
   props (caps, glasses, weapons) can follow the body.
3. **Edit** the meme in the GUI: timeline, background tracks,
   chroma-keyed actors, attached props, text overlays and camera moves.
4. **Render** out a 1080×1920 / 60 fps MP4 ready for Shorts/Reels/TikTok.

The editor UI is fully bilingual — English / Russian — and the
language picker lives in `Settings → Language`. Every parameter,
button, modifier and inspector tab has been translated, so a Russian
release build is comfortable to use without falling back to English
for "just one more setting".

## Repository layout

```
memstroy-inator/                # was: memstroy_generator/
├── Cargo.toml                 # workspace root
├── crates/
│   ├── memstroy-core/             # Scene + animation model (serde-driven)
│   ├── memstroy-tg/               # Public-channel scraper + downloader
│   ├── memstroy-vision/           # Chroma key (CPU) + pose-anchor trait
│   ├── memstroy-render/           # Scene → FFmpeg filter graph → MP4
│   ├── memstroy-cli/              # `memstroy` CLI binary
│   ├── memstroy-assets-server/    # HTTP backend that indexes/serves assets
│   └── memstroy-gui/              # `memstroy-gui` editor (eframe/egui)
├── examples/
│   └── scene.yaml                 # starter scene
└── scripts/
    ├── package-client.sh          # build + bundle a client release
    ├── package-client.ps1         # same, on Windows PowerShell
    ├── start-server.sh            # launch the standalone backend
    └── start-server.ps1           # same, on Windows PowerShell
```

## Build prerequisites

- **Rust** stable toolchain (`rustup install stable`). See
  `rust-toolchain.toml` — the workspace pins `channel = "stable"`,
  so `rustup show` is enough.
- A C toolchain (`gcc`, `pkg-config`) and `openssl-devel` (or
  equivalent) for the dependent crates.
- **ALSA dev headers** on Linux (e.g. `alsa-lib-devel` on Fedora /
  Amazon Linux, `libasound2-dev` on Debian / Ubuntu) — `rodio` links
  against ALSA for desktop audio.
- **FFmpeg 6+** in `$PATH` or pointed to via the `MEMSTROY_FFMPEG`
  environment variable. Used both by the renderer and by the GUI's
  preview-frame extractor.
- **Linux GUI**: an X11 or Wayland session, plus `libxkbcommon`. On
  a server use the CLI; the GUI needs a display.

## How it fits together: GUI ↔ backend

The editor (`memstroy-gui`) and the headless renderer
(`memstroy-render`) both talk to a small HTTP backend
(`memstroy-assets-server`) that owns the on-disk asset library
(clips / videos / images / sounds / particles / text snippets).

By default the GUI **auto-spawns** that backend in-process, on the
same Tokio runtime, listening on `127.0.0.1:8765` and indexing
`./assets/` from the directory where the editor was started. Users
typically don't need to launch the server by hand — opening
`memstroy-gui` is enough.

The standalone server is still useful when:

- two or more developers share a single asset library over a LAN,
- a render farm pulls clips without booting the GUI,
- you want to re-ingest from Telegram on a schedule without leaving
  the editor open.

In those cases run the binary directly (see the *Backend* section
below) and point the GUI at the network address through
`Settings → Server URL`.

## Packaging the client (release build)

The `scripts/` directory ships a self-contained packager that
produces a release-stripped, asset-bundled folder ready to be
zipped and shipped:

```bash
# Linux / macOS
scripts/package-client.sh                        # → dist/memstroy-inator-<os>-<ver>/

# Windows PowerShell
pwsh scripts/package-client.ps1                  # → dist\memstroy-inator-windows-<ver>\
```

What the script does:

1. `cargo build --release -p memstroy-gui -p memstroy-assets-server -p memstroy-cli`.
2. Copies the three release binaries into `dist/<bundle-name>/bin/`.
3. Mirrors the runtime asset skeleton (`assets/images`,
   `assets/sounds`, `assets/particles`, `assets/clips`,
   `assets/videos`, `assets/text`) so the GUI has somewhere to put
   downloaded clips on first launch.
4. Drops an `examples/` copy, the README and a top-level
   launcher script (`memstroy-inator.sh` / `memstroy-inator.bat`).

Override the output directory with `--out <path>` and the bundle
name with `--name <name>` if you want to ship a specific build to a
specific environment.

## Running the backend (assets-server)

Use the provided launcher:

```bash
# Linux / macOS — defaults: 0.0.0.0:8765, asset root = ./assets
scripts/start-server.sh

# bind a different address / asset root
scripts/start-server.sh --addr 127.0.0.1:9000 --root /var/lib/memstroy/assets

# Windows PowerShell
pwsh scripts/start-server.ps1 -Addr 127.0.0.1:9000 -Root C:\memstroy\assets
```

Or invoke the binary directly:

```bash
cargo run -p memstroy-assets-server --release -- \
    --addr 0.0.0.0:8765 \
    --root ./assets
```

The server creates any missing kind subdirectories (`clips/`,
`videos/`, `images/`, `sounds/`, `particles/`, `text/`) on startup
and re-indexes after every successful Telegram ingest. The HTTP
surface is documented in
[`crates/memstroy-assets-server/src/lib.rs`](crates/memstroy-assets-server/src/lib.rs).

The default tracing filter is `info`. Override it with
`RUST_LOG=memstroy_assets_server=debug` for ingest debugging.

## CLI usage

```bash
# 1. Download every "Имба" clip into ./assets/mellstroy
cargo run -p memstroy-cli --release -- download

# 2. Generate a starter scene file
cargo run -p memstroy-cli --release -- new my_scene.yaml

# 3. Render to MP4 (assets paths are resolved relative to --assets)
cargo run -p memstroy-cli --release -- render my_scene.yaml \
    -o out.mp4 --assets .

# 4. Pull a single PNG preview frame at t=2s
cargo run -p memstroy-cli --release -- preview my_scene.yaml \
    -o frame.png -t 2.0
```

`memstroy --help` and `memstroy <subcommand> --help` document every
flag (filter, page cap, concurrency, catalog-only mode, ML matting
model path, …).

## GUI

```bash
cargo run -p memstroy-gui --release
```

The editor opens with five regions:

- **Top menu** — File / Render / View.
- **Library (left)** — `Clips`, `Videos`, `Sounds`, `Images`,
  `Particles`. The Clips tab is server-driven; the others scan the
  matching `assets/<kind>/` directory directly. Drag a row onto the
  canvas or the timeline to add it to the scene.
- **Preview (centre)** — a 9:16 canvas at the output ratio with
  live transform handles, chroma-key picker and effect stack.
- **Inspector (right)** — properties for the selected actor, overlay,
  background or audio track. Animation keyframes are added/edited
  inline; modifiers (wobble / shake / pulse / spin / walk) and the
  professional colour-correction panel (lift / gamma / gain wheels +
  master / R / G / B curves) live here too.
- **Timeline (bottom)** — multi-lane timeline with razor, snap,
  loop, video-/audio-layer creation buttons, and per-parameter
  keyframe strips.

Use `Settings → Language` to switch between English and Русский.

## Scene format (excerpt)

A scene is a YAML or JSON file with `format_version: 1`. See
`examples/scene.yaml` for a full sample. All animatable values follow
the same shape:

```yaml
layout:
  - t: 0.0
    value: { pos: [0.5, 0.7], scale: 1.0 }
    easing: linear
  - t: 1.5
    value: { pos: [0.3, 0.5], scale: 1.2 }
    easing: ease_out
```

`pos` is in normalised scene coordinates (`[0, 1]`), so the same scene
re-renders correctly at any output resolution.

## License

MIT. See per-crate `Cargo.toml` for the canonical declaration.
