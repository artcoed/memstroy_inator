#!/usr/bin/env bash
# scripts/package-client.sh
#
# Build a hardened release bundle of the Memstroy-inator editor for
# distribution to clients. The output is a self-contained directory
# the operator can zip and ship.
#
# Threat model (Level 1):
#   * The shipped binary should not embed application sources, panic
#     paths or symbols. We rely on the workspace `[profile.release]`
#     for `strip = "symbols"` / `panic = "abort"` / no debug-info, and
#     run a belt-and-braces `strip --strip-all` post-link on Linux.
#   * The default assets-server URL the editor connects to is baked
#     at compile time into the binary via `obfstr` so it does not show
#     up in `strings(1)` over the artefact. Override it per-bundle
#     with `--server-url`.
#
# What the bundle ships:
#   * `bin/memstroy-gui` (+ `bin/memstroy` CLI)
#   * `models/u2netp.onnx` — AI background removal (canvas cutout tool)
#   * `examples/`, `README.md` and a small launcher `Memstroy-inator.sh`.
#
# What the bundle deliberately does NOT ship:
#   * The full in-tree `assets/` directory (clips/images/sounds are
#     fetched from the operator's remote memstroy-assets-server and
#     cached under `~/.memstroy/cache/` on first use).
#   * The `memstroy-assets-server` binary itself. The server is run by
#     the operator on their backend, not by the client. Use
#     `scripts/start-server.sh` separately for that.
#
# Usage:
#   scripts/package-client.sh --server-url https://assets.example.com
#   scripts/package-client.sh --server-url https://assets.example.com --zip
#   scripts/package-client.sh --server-url https://assets.example.com \
#                             --out ./build --name memstroy-1.2.3
#
# Required:
#   --server-url <URL>   Backend the shipped editor talks to. Refusing
#                        to default this protects against accidentally
#                        shipping a build that hits 127.0.0.1.
#
# Optional:
#   --out <path>         Output directory (default: ./dist).
#   --name <name>        Bundle name (default:
#                        Memstroy-inator-<os>-<arch>-<version>).
#   --zip                Also produce <bundle-name>.zip alongside the
#                        directory.
#   --allow-loopback     Allow `--server-url` to point at 127.* / ::1
#                        / localhost. By default that's an error to
#                        catch typos; pass this flag for staging
#                        bundles that intentionally hit a local box.
#
# All paths are resolved relative to the workspace root, regardless of
# the current working directory.
set -euo pipefail

# ─── Locate the workspace root ───────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${ROOT_DIR}"

# ─── Defaults / CLI ──────────────────────────────────────────────────
OS_NAME="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH_NAME="$(uname -m)"
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/' || echo "dev")"
DEFAULT_NAME="Memstroy-inator-${OS_NAME}-${ARCH_NAME}-${VERSION}"

OUT_DIR="${ROOT_DIR}/dist"
BUNDLE_NAME="${DEFAULT_NAME}"
SERVER_URL=""
ALLOW_LOOPBACK=0
MAKE_ZIP=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --out)             OUT_DIR="$2"; shift 2 ;;
        --name)            BUNDLE_NAME="$2"; shift 2 ;;
        --server-url)      SERVER_URL="$2"; shift 2 ;;
        --zip)             MAKE_ZIP=1; shift ;;
        --allow-loopback)  ALLOW_LOOPBACK=1; shift ;;
        -h|--help)
            sed -n '2,55p' "$0"
            exit 0
            ;;
        *)
            echo "unknown option: $1" >&2
            exit 1
            ;;
    esac
done

if [[ -z "${SERVER_URL}" ]]; then
    echo "error: --server-url is required (e.g. --server-url https://assets.example.com)" >&2
    echo "       use --allow-loopback if you genuinely want a local-box bundle" >&2
    exit 2
fi

# ─── Loopback / scheme guard ─────────────────────────────────────────
# Without this guard a tired engineer easily ships a bundle whose
# baked URL points at their laptop. The check is intentionally simple
# (substring match) so it stays trivially auditable.
if [[ "${ALLOW_LOOPBACK}" -eq 0 ]]; then
    case "${SERVER_URL}" in
        *127.0.0.1*|*localhost*|*"::1"*|*0.0.0.0*)
            echo "error: --server-url points at a loopback host (${SERVER_URL})." >&2
            echo "       Pass --allow-loopback if this is intentional." >&2
            exit 3
            ;;
    esac
fi
case "${SERVER_URL}" in
    http://*|https://*) ;;
    *)
        echo "error: --server-url must start with http:// or https:// (got: ${SERVER_URL})" >&2
        exit 4
        ;;
esac

BUNDLE_DIR="${OUT_DIR}/${BUNDLE_NAME}"

echo "==> Memstroy-inator client packager"
echo "    workspace  : ${ROOT_DIR}"
echo "    bundle     : ${BUNDLE_DIR}"
echo "    server URL : ${SERVER_URL}"

# ─── Build release binaries ──────────────────────────────────────────
# We pass the build-time signals through env vars so `build.rs` can
# bake the obfstr-wrapped URL and the IS_CLIENT_BUILD flag into the
# artefact. `cargo build` automatically reruns the build script when
# either env var changes (see `cargo:rerun-if-env-changed=` in
# `crates/memstroy-gui/build.rs`).
echo "==> cargo build --release (client mode)"
export MEMSTROY_CLIENT_BUILD=1
export MEMSTROY_DEFAULT_SERVER_URL="${SERVER_URL}"
cargo build --release \
    -p memstroy-gui \
    -p memstroy-cli \
    --no-default-features

# ─── Stage the bundle ────────────────────────────────────────────────
echo "==> staging bundle"
rm -rf "${BUNDLE_DIR}"
mkdir -p "${BUNDLE_DIR}/bin"

# Note: the assets-server binary is intentionally NOT shipped — it
# lives on the operator's backend, not on the client.
for bin in memstroy-gui memstroy; do
    src="target/release/${bin}"
    if [[ ! -x "${src}" ]]; then
        echo "missing release binary: ${src}" >&2
        exit 1
    fi
    cp "${src}" "${BUNDLE_DIR}/bin/"
done

# ─── Bundled FFmpeg ──────────────────────────────────────────────────
resolve_required_tool() {
    local tool_name="$1"
    local env_path="${2:-}"
    local repo_candidate="${ROOT_DIR}/tools/ffmpeg/bin/${tool_name}"

    if [[ -n "${env_path}" && -x "${env_path}" ]]; then
        printf '%s\n' "${env_path}"
        return 0
    fi
    if [[ -x "${repo_candidate}" ]]; then
        printf '%s\n' "${repo_candidate}"
        return 0
    fi
    if command -v "${tool_name}" >/dev/null 2>&1; then
        command -v "${tool_name}"
        return 0
    fi

    echo "missing ${tool_name}; install FFmpeg on the build machine, set MEMSTROY_FFMPEG/MEMSTROY_FFPROBE, or place binaries in tools/ffmpeg/bin" >&2
    exit 1
}

validate_real_ffmpeg_tool() {
    local tool_path="$1"
    local tool_name="$2"
    local size

    size="$(wc -c < "${tool_path}" | tr -d '[:space:]')"
    if ! "${tool_path}" -version >/dev/null 2>&1; then
        echo "error: ${tool_name} failed '-version' check: ${tool_path}" >&2
        exit 1
    fi
    if [[ "${size}" -lt 1048576 ]]; then
        echo "warning: ${tool_name} is smaller than a static build (${size} bytes): ${tool_path}" >&2
        echo "         continuing because '${tool_name} -version' works" >&2
    fi
}

FFMPEG_SRC="$(resolve_required_tool ffmpeg "${MEMSTROY_FFMPEG:-}")"
if [[ -n "${MEMSTROY_FFPROBE:-}" ]]; then
    FFPROBE_SRC="$(resolve_required_tool ffprobe "${MEMSTROY_FFPROBE}")"
elif [[ -x "$(dirname "${FFMPEG_SRC}")/ffprobe" ]]; then
    FFPROBE_SRC="$(dirname "${FFMPEG_SRC}")/ffprobe"
else
    FFPROBE_SRC="$(resolve_required_tool ffprobe)"
fi
validate_real_ffmpeg_tool "${FFMPEG_SRC}" ffmpeg
validate_real_ffmpeg_tool "${FFPROBE_SRC}" ffprobe
cp "${FFMPEG_SRC}" "${BUNDLE_DIR}/bin/ffmpeg"
cp "${FFPROBE_SRC}" "${BUNDLE_DIR}/bin/ffprobe"
echo "    bundled   : bin/ffmpeg"
echo "    bundled   : bin/ffprobe"

# ─── AI background-removal model (U²-Netp) ───────────────────────────
MODEL_SRC="${ROOT_DIR}/assets/models/u2netp.onnx"
if [[ ! -f "${MODEL_SRC}" ]]; then
    echo "error: missing AI model: ${MODEL_SRC} (place u2netp.onnx there before packaging)" >&2
    exit 1
fi
mkdir -p "${BUNDLE_DIR}/models"
cp "${MODEL_SRC}" "${BUNDLE_DIR}/models/u2netp.onnx"
echo "    bundled   : models/u2netp.onnx"

# ─── Belt-and-braces strip (Linux/macOS) ─────────────────────────────
# `[profile.release]` already sets `strip = "symbols"`, so this is a
# no-op on a fully cooperating toolchain. It guards against a future
# toolchain default change that re-emits a symbol table.
if command -v strip >/dev/null 2>&1; then
    case "${OS_NAME}" in
        linux*)
            echo "==> post-link strip (Linux)"
            strip --strip-all "${BUNDLE_DIR}/bin/memstroy-gui" "${BUNDLE_DIR}/bin/memstroy" 2>/dev/null || true
            ;;
        darwin*)
            echo "==> post-link strip (macOS)"
            strip -S -x "${BUNDLE_DIR}/bin/memstroy-gui" "${BUNDLE_DIR}/bin/memstroy" 2>/dev/null || true
            ;;
    esac
fi

# ─── Examples + docs ─────────────────────────────────────────────────
mkdir -p "${BUNDLE_DIR}/examples"
cp examples/*.yaml "${BUNDLE_DIR}/examples/" 2>/dev/null || true
cp README.md "${BUNDLE_DIR}/" 2>/dev/null || true

# ─── App icon ────────────────────────────────────────────────────────
# The Linux installer (scripts/make-installer.sh) installs this PNG
# under ${INSTALL_DIR}/share/icons/ and references it from the
# generated .desktop file's `Icon=` line so the menu entry / desktop
# shortcut display the branded logo. We also carry the .ico in case
# the same bundle ever needs to be repackaged for Windows on a
# non-Windows host.
ICON_PNG_SRC="${ROOT_DIR}/assets/internal_images/catost.png"
ICON_ICO_SRC="${ROOT_DIR}/assets/internal_images/catost.ico"
if [[ -f "${ICON_PNG_SRC}" ]]; then
    cp "${ICON_PNG_SRC}" "${BUNDLE_DIR}/catost.png"
else
    echo "warning: app icon not found at ${ICON_PNG_SRC}; menu entry will use the generic icon" >&2
fi
if [[ -f "${ICON_ICO_SRC}" ]]; then
    cp "${ICON_ICO_SRC}" "${BUNDLE_DIR}/catost.ico"
fi

# ─── Top-level launcher ──────────────────────────────────────────────
# The launcher no longer cd's into the bundle to find a local `assets/`
# directory — there isn't one in client mode. The editor reads its
# cache from `~/.memstroy/cache/` regardless of where the launcher was
# invoked from, so we just exec the binary.
cat > "${BUNDLE_DIR}/Memstroy-inator.sh" <<'LAUNCH'
#!/usr/bin/env bash
# Launch the Memstroy-inator editor.
#
# All assets are fetched from the configured assets-server on demand
# and cached under ~/.memstroy/cache/. Override the server URL through
# the editor's Settings dialog if the operator has migrated their
# backend.
set -e
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "${SCRIPT_DIR}/bin/memstroy-gui" "$@"
LAUNCH
chmod +x "${BUNDLE_DIR}/Memstroy-inator.sh"

# ─── Optional zip ────────────────────────────────────────────────────
if [[ "${MAKE_ZIP}" -eq 1 ]]; then
    echo "==> zipping bundle"
    (cd "${OUT_DIR}" && zip -qr "${BUNDLE_NAME}.zip" "${BUNDLE_NAME}")
    echo "    archive : ${OUT_DIR}/${BUNDLE_NAME}.zip"
fi

echo "==> done"
echo "    run with: ${BUNDLE_DIR}/Memstroy-inator.sh"
