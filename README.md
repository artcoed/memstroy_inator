# memstroy_generator

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

> Status: skeleton in place. The downloader, scene model, FFmpeg-based
> renderer and the GUI shell compile and run. Pose detection, GPU
> preview compositor and timeline drag-editing are queued for the next
> iterations (see *Roadmap* below).

## Repository layout

```
memstroy_generator/
├── Cargo.toml                 # workspace root
├── crates/
│   ├── memstroy-core/         # Scene + animation model (serde-driven)
│   ├── memstroy-tg/           # Public-channel scraper + downloader
│   ├── memstroy-vision/       # Chroma key (CPU) + pose-anchor trait
│   ├── memstroy-render/       # Scene → FFmpeg filter graph → MP4
│   ├── memstroy-cli/          # `memstroy` CLI binary
│   └── memstroy-gui/          # `memstroy-gui` editor (eframe/egui)
└── examples/
    └── scene.yaml             # starter scene
```

## Build prerequisites

- Rust **stable** toolchain (`rustup install stable`).
- A C toolchain (`gcc`, `pkg-config`) and `openssl-devel` (or
  equivalent) for the dependent crates.
- **FFmpeg 6+** in `$PATH` or pointed to via `MEMSTROY_FFMPEG`.
- Linux GUI: X11 or Wayland, plus `libxkbcommon`. On a server use the
  CLI; the GUI needs a display.

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

`memstroy download --help` documents every flag (filter, page cap,
concurrency, catalog-only mode).

## GUI

```bash
cargo run -p memstroy-gui --release
```

The editor opens with five regions:

- **Top menu** — File / Channel / Render / Tools.
- **Library (left)** — clips found in `assets/mellstroy/`,
  backgrounds in `assets/backgrounds/`, props in `assets/props/`.
  Click `+` to add an asset to the scene.
- **Preview (centre)** — a 9:16 placeholder at the output ratio.
  *Render → Render preview frame* asks FFmpeg for a still at the
  current playhead and shows it here.
- **Inspector (right)** — properties for the selected actor, overlay
  or background. Animation keyframes are added/edited inline.
- **Timeline (bottom)** — list of layers + a playhead slider. Real
  drag-to-edit timeline tracks are coming next.

Channel → Download from Telegram opens a dialog that runs the same
scraper as the CLI in the background and refreshes the library.

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

## Roadmap

| Iteration | Scope                                                                  |
|----------:|------------------------------------------------------------------------|
|         1 | **(this commit)** Workspace skeleton, scraper, scene model, GUI shell, FFmpeg filter graph render, CLI |
|         2 | Pose estimation backend (ONNX Runtime + MoveNet/YOLO11-pose), anchor JSON cache, `Detect anchors` tool in GUI |
|         3 | Pose-driven attachment compositor (props follow head/wrists), spill suppression, frame-pump fallback for complex animations |
|         4 | Camera moves (zoom/pan kf), `Snap` and `Slide*` transitions, audio mixer UI |
|         5 | Real timeline drag-editing in the GUI, drag-and-drop import, undo/redo |
|         6 | GPU preview compositor (wgpu) for live scrubbing without round-tripping FFmpeg |

Each iteration is a small enough chunk that we can ship a runnable
binary at the end of it.
