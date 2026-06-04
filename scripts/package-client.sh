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
#   --fetch-ffmpeg       Linux only: download static ffmpeg/ffprobe into
#                        tools/ffmpeg/bin before staging the bundle.
#   --allow-dynamic-ffmpeg
#                        Linux only: allow a dynamically-linked FFmpeg
#                        pair. Off by default because the Linux .run is
#                        meant to work without distro FFmpeg packages.
#   --no-bundle-libs     Linux only: do not copy non-glibc ELF runtime
#                        libraries into bundle/lib.
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
FETCH_FFMPEG=0
ALLOW_DYNAMIC_FFMPEG=0
BUNDLE_LINUX_LIBS=1

while [[ $# -gt 0 ]]; do
    case "$1" in
        --out)             OUT_DIR="$2"; shift 2 ;;
        --name)            BUNDLE_NAME="$2"; shift 2 ;;
        --server-url)      SERVER_URL="$2"; shift 2 ;;
        --zip)             MAKE_ZIP=1; shift ;;
        --fetch-ffmpeg)    FETCH_FFMPEG=1; shift ;;
        --allow-dynamic-ffmpeg) ALLOW_DYNAMIC_FFMPEG=1; shift ;;
        --no-bundle-libs)  BUNDLE_LINUX_LIBS=0; shift ;;
        --allow-loopback)  ALLOW_LOOPBACK=1; shift ;;
        -h|--help)
            sed -n '2,64p' "$0"
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
if [[ "${FETCH_FFMPEG}" -eq 1 ]]; then
    if [[ "${OS_NAME}" != linux* ]]; then
        echo "error: --fetch-ffmpeg is only supported on Linux build hosts" >&2
        exit 1
    fi
    echo "==> fetching static Linux FFmpeg"
    "${SCRIPT_DIR}/fetch-static-ffmpeg-linux.sh"
fi

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
    local ldd_out

    size="$(wc -c < "${tool_path}" | tr -d '[:space:]')"
    if ! "${tool_path}" -version >/dev/null 2>&1; then
        echo "error: ${tool_name} failed '-version' check: ${tool_path}" >&2
        exit 1
    fi
    if [[ "${OS_NAME}" == linux* && "${ALLOW_DYNAMIC_FFMPEG}" -eq 0 ]] && command -v ldd >/dev/null 2>&1; then
        ldd_out="$(ldd "${tool_path}" 2>&1 || true)"
        if [[ "${ldd_out}" != *"not a dynamic executable"* && "${ldd_out}" != *"statically linked"* ]]; then
            echo "error: ${tool_name} is dynamically linked: ${tool_path}" >&2
            echo "       Run scripts/fetch-static-ffmpeg-linux.sh or pass --fetch-ffmpeg." >&2
            echo "       Use --allow-dynamic-ffmpeg only for non-portable test bundles." >&2
            exit 1
        fi
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

# ─── Linux ELF runtime libraries ─────────────────────────────────────
# Client Linux machines should not have to install most app-facing
# runtime libraries. We copy portable non-glibc dynamic dependencies
# reported by ldd into bundle/lib and launch the app through a wrapper
# that prepends this directory to LD_LIBRARY_PATH.
#
# We intentionally do NOT bundle glibc, the dynamic loader, or GPU
# driver/vendor libraries. Those belong to the target OS and graphics
# stack; shipping build-host copies is more likely to break machines
# than help them. Build Linux releases on an old-enough distro (for
# example Ubuntu 20.04/22.04) to keep the glibc floor broad.
should_bundle_linux_lib() {
    local lib_path="$1"
    local name
    name="$(basename "${lib_path}")"

    case "${name}" in
        linux-vdso*|ld-linux*|ld-musl*|libc.so.*|libm.so.*|libdl.so.*|librt.so.*|libpthread.so.*|libresolv.so.*|libutil.so.*|libnsl.so.*|libanl.so.*)
            return 1
            ;;
        # ALSA loads PulseAudio/PipeWire/rate-converter plugins from the
        # target system at runtime. Shipping a build-host libasound without
        # its matching plugin set can make preview audio fail on otherwise
        # healthy Linux desktops.
        libasound.so.*)
            return 1
            ;;
        libGLX_nvidia*|libEGL_nvidia*|libnvidia-*|libcuda*|libOpenCL*|libdrm*|libgbm*|libva*|libvdpau*|libwayland-egl*|libEGL_mesa*|libGLX_mesa*)
            return 1
            ;;
    esac

    return 0
}

bundle_linux_elf_dependencies() {
    if [[ "${OS_NAME}" != linux* || "${BUNDLE_LINUX_LIBS}" -eq 0 ]]; then
        return 0
    fi
    if ! command -v ldd >/dev/null 2>&1; then
        echo "error: ldd is required to bundle Linux runtime libraries" >&2
        exit 1
    fi

    echo "==> bundling Linux ELF runtime libraries"
    local lib_dir="${BUNDLE_DIR}/lib"
    local manifest="${lib_dir}/manifest.txt"
    local target line path name copied missing
    mkdir -p "${lib_dir}"
    : > "${manifest}"

    declare -A seen_libs=()
    copied=0
    missing=0

    for target in "${BUNDLE_DIR}/bin/memstroy-gui" "${BUNDLE_DIR}/bin/memstroy"; do
        while IFS= read -r line; do
            if [[ "${line}" == *"not found"* ]]; then
                echo "error: unresolved ELF dependency for ${target}: ${line}" >&2
                missing=1
                continue
            fi

            path=""
            if [[ "${line}" =~ \=\>[[:space:]]*(/[^[:space:]]+) ]]; then
                path="${BASH_REMATCH[1]}"
            elif [[ "${line}" =~ ^[[:space:]]*(/[^[:space:]]+) ]]; then
                path="${BASH_REMATCH[1]}"
            fi

            if [[ -z "${path}" || ! -f "${path}" ]]; then
                continue
            fi
            if ! should_bundle_linux_lib "${path}"; then
                continue
            fi

            name="$(basename "${path}")"
            if [[ -n "${seen_libs[${name}]:-}" ]]; then
                continue
            fi

            cp -L "${path}" "${lib_dir}/${name}"
            chmod 0644 "${lib_dir}/${name}" 2>/dev/null || true
            seen_libs["${name}"]="${path}"
            printf '%s <- %s\n' "${name}" "${path}" >> "${manifest}"
            copied=$((copied + 1))
        done < <(ldd "${target}")
    done

    if [[ "${missing}" -ne 0 ]]; then
        exit 1
    fi

    if [[ "${copied}" -eq 0 ]]; then
        rm -f "${manifest}"
        rmdir "${lib_dir}" 2>/dev/null || true
        echo "    bundled   : no extra ELF libraries needed"
    else
        echo "    bundled   : ${copied} library file(s) in lib/"
        echo "    manifest  : lib/manifest.txt"
    fi
}

bundle_linux_elf_dependencies

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
# directory - there isn't one in client mode. It does set up the
# bundled Linux runtime surface (PATH, LD_LIBRARY_PATH, FFmpeg env
# vars), so desktop entries and ~/.local/bin symlinks must point at
# this wrapper instead of directly at bin/memstroy-gui.
cat > "${BUNDLE_DIR}/Memstroy-inator.sh" <<'LAUNCH'
#!/usr/bin/env bash
# Launch the Memstroy-inator editor.
#
# All assets are fetched from the configured assets-server on demand
# and cached under ~/.memstroy/cache/. Override the server URL through
# the editor's Settings dialog if the operator has migrated their
# backend.
set -e
SOURCE="${BASH_SOURCE[0]}"
while [[ -L "${SOURCE}" ]]; do
    DIR="$(cd -P "$(dirname "${SOURCE}")" && pwd)"
    SOURCE="$(readlink "${SOURCE}")"
    [[ "${SOURCE}" != /* ]] && SOURCE="${DIR}/${SOURCE}"
done
SCRIPT_DIR="$(cd -P "$(dirname "${SOURCE}")" && pwd)"

export PATH="${SCRIPT_DIR}/bin${PATH:+:${PATH}}"
if [[ -d "${SCRIPT_DIR}/lib" ]]; then
    export LD_LIBRARY_PATH="${SCRIPT_DIR}/lib${LD_LIBRARY_PATH:+:${LD_LIBRARY_PATH}}"
fi
if [[ -x "${SCRIPT_DIR}/bin/ffmpeg" ]]; then
    export MEMSTROY_FFMPEG="${SCRIPT_DIR}/bin/ffmpeg"
fi
if [[ -x "${SCRIPT_DIR}/bin/ffprobe" ]]; then
    export MEMSTROY_FFPROBE="${SCRIPT_DIR}/bin/ffprobe"
fi

exec "${SCRIPT_DIR}/bin/memstroy-gui" "$@"
LAUNCH
chmod +x "${BUNDLE_DIR}/Memstroy-inator.sh"

cat > "${BUNDLE_DIR}/Memstroy-inator-safe-graphics.sh" <<'LAUNCH'
#!/usr/bin/env bash
# Launch through the WGPU path for machines where the default OpenGL
# window opens black.
set -e
SOURCE="${BASH_SOURCE[0]}"
while [[ -L "${SOURCE}" ]]; do
    DIR="$(cd -P "$(dirname "${SOURCE}")" && pwd)"
    SOURCE="$(readlink "${SOURCE}")"
    [[ "${SOURCE}" != /* ]] && SOURCE="${DIR}/${SOURCE}"
done
SCRIPT_DIR="$(cd -P "$(dirname "${SOURCE}")" && pwd)"
exec "${SCRIPT_DIR}/Memstroy-inator.sh" --graphics=safe "$@"
LAUNCH
chmod +x "${BUNDLE_DIR}/Memstroy-inator-safe-graphics.sh"

cat > "${BUNDLE_DIR}/memstroy.sh" <<'LAUNCH'
#!/usr/bin/env bash
# Launch the bundled CLI with the same runtime environment as the GUI.
set -e
SOURCE="${BASH_SOURCE[0]}"
while [[ -L "${SOURCE}" ]]; do
    DIR="$(cd -P "$(dirname "${SOURCE}")" && pwd)"
    SOURCE="$(readlink "${SOURCE}")"
    [[ "${SOURCE}" != /* ]] && SOURCE="${DIR}/${SOURCE}"
done
SCRIPT_DIR="$(cd -P "$(dirname "${SOURCE}")" && pwd)"

export PATH="${SCRIPT_DIR}/bin${PATH:+:${PATH}}"
if [[ -d "${SCRIPT_DIR}/lib" ]]; then
    export LD_LIBRARY_PATH="${SCRIPT_DIR}/lib${LD_LIBRARY_PATH:+:${LD_LIBRARY_PATH}}"
fi
if [[ -x "${SCRIPT_DIR}/bin/ffmpeg" ]]; then
    export MEMSTROY_FFMPEG="${SCRIPT_DIR}/bin/ffmpeg"
fi
if [[ -x "${SCRIPT_DIR}/bin/ffprobe" ]]; then
    export MEMSTROY_FFPROBE="${SCRIPT_DIR}/bin/ffprobe"
fi

exec "${SCRIPT_DIR}/bin/memstroy" "$@"
LAUNCH
chmod +x "${BUNDLE_DIR}/memstroy.sh"

# ─── Optional zip ────────────────────────────────────────────────────
if [[ "${MAKE_ZIP}" -eq 1 ]]; then
    echo "==> zipping bundle"
    (cd "${OUT_DIR}" && zip -qr "${BUNDLE_NAME}.zip" "${BUNDLE_NAME}")
    echo "    archive : ${OUT_DIR}/${BUNDLE_NAME}.zip"
fi

echo "==> done"
echo "    run with: ${BUNDLE_DIR}/Memstroy-inator.sh"
