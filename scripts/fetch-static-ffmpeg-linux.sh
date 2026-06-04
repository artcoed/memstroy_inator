#!/usr/bin/env bash
# Download a static Linux FFmpeg/FFprobe pair for client bundles.
#
# This is a build-machine helper, not a target-machine installer step:
# the produced .run carries bin/ffmpeg and bin/ffprobe inside itself, so
# end users do not need to install FFmpeg through apt/dnf/pacman.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

OUT_DIR="${ROOT_DIR}/tools/ffmpeg/bin"
ARCH="$(uname -m)"
FORCE=0
CUSTOM_URL=""

usage() {
    sed -n '2,24p' "$0"
    cat <<'EOF'

Usage:
  scripts/fetch-static-ffmpeg-linux.sh
  scripts/fetch-static-ffmpeg-linux.sh --force
  scripts/fetch-static-ffmpeg-linux.sh --out ./tools/ffmpeg/bin

Options:
  --out <path>   Where to place ffmpeg and ffprobe.
  --arch <arch>  Override detected arch: x86_64/amd64 or aarch64/arm64.
  --url <url>    Override the archive URL.
  --force        Re-download even when both tools already exist.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --out)   OUT_DIR="$2"; shift 2 ;;
        --arch)  ARCH="$2"; shift 2 ;;
        --url)   CUSTOM_URL="$2"; shift 2 ;;
        --force) FORCE=1; shift ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "unknown option: $1" >&2
            exit 1
            ;;
    esac
done

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "error: this helper downloads Linux ELF binaries; run it on Linux." >&2
    exit 2
fi

mkdir -p "${OUT_DIR}"
OUT_DIR="$(cd "${OUT_DIR}" && pwd)"

if [[ "${FORCE}" -eq 0 && -x "${OUT_DIR}/ffmpeg" && -x "${OUT_DIR}/ffprobe" ]]; then
    if "${OUT_DIR}/ffmpeg" -version >/dev/null 2>&1 && "${OUT_DIR}/ffprobe" -version >/dev/null 2>&1; then
        echo "==> static FFmpeg tools already present"
        echo "    ffmpeg : ${OUT_DIR}/ffmpeg"
        echo "    ffprobe: ${OUT_DIR}/ffprobe"
        exit 0
    fi
fi

case "${ARCH}" in
    x86_64|amd64)  FFMPEG_ARCH="amd64" ;;
    aarch64|arm64) FFMPEG_ARCH="arm64" ;;
    *)
        echo "error: unsupported Linux arch '${ARCH}'." >&2
        echo "       Supported: x86_64/amd64 and aarch64/arm64." >&2
        exit 3
        ;;
esac

URL="${CUSTOM_URL:-https://johnvansickle.com/ffmpeg/releases/ffmpeg-release-${FFMPEG_ARCH}-static.tar.xz}"

if command -v curl >/dev/null 2>&1; then
    download() { curl -fL --retry 3 --retry-delay 2 -o "$1" "$2"; }
elif command -v wget >/dev/null 2>&1; then
    download() { wget -O "$1" "$2"; }
else
    echo "error: need curl or wget on the build machine to download FFmpeg." >&2
    exit 4
fi

if ! tar --help 2>/dev/null | grep -q -- '-J'; then
    echo "warning: your tar may not support .tar.xz directly; extraction will still be attempted." >&2
fi

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "${TMP_DIR}"' EXIT

ARCHIVE="${TMP_DIR}/ffmpeg-static.tar.xz"

echo "==> downloading static FFmpeg"
echo "    url : ${URL}"
download "${ARCHIVE}" "${URL}"

echo "==> extracting"
tar -xJf "${ARCHIVE}" -C "${TMP_DIR}"

FFMPEG_SRC="$(find "${TMP_DIR}" -type f -name ffmpeg -perm -111 | head -n 1)"
FFPROBE_SRC="$(find "${TMP_DIR}" -type f -name ffprobe -perm -111 | head -n 1)"

if [[ -z "${FFMPEG_SRC}" || -z "${FFPROBE_SRC}" ]]; then
    echo "error: archive did not contain executable ffmpeg and ffprobe." >&2
    exit 5
fi

install -m 0755 "${FFMPEG_SRC}" "${OUT_DIR}/ffmpeg"
install -m 0755 "${FFPROBE_SRC}" "${OUT_DIR}/ffprobe"

"${OUT_DIR}/ffmpeg" -version >/dev/null
"${OUT_DIR}/ffprobe" -version >/dev/null

echo "==> done"
echo "    ffmpeg : ${OUT_DIR}/ffmpeg"
echo "    ffprobe: ${OUT_DIR}/ffprobe"
