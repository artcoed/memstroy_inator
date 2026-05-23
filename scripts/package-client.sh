#!/usr/bin/env bash
# scripts/package-client.sh
#
# Build a release binary set for the memstroy-inator client and bundle
# it together with the runtime asset skeleton, the example scene and a
# small launcher script. The output is a self-contained directory the
# user can zip and ship.
#
# Usage:
#   scripts/package-client.sh                       # default bundle
#   scripts/package-client.sh --out ./build         # custom output dir
#   scripts/package-client.sh --name memstroy-1.2.3 # custom bundle name
#   scripts/package-client.sh --zip                 # also produce a .zip
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
DEFAULT_NAME="memstroy-inator-${OS_NAME}-${ARCH_NAME}-${VERSION}"

OUT_DIR="${ROOT_DIR}/dist"
BUNDLE_NAME="${DEFAULT_NAME}"
MAKE_ZIP=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --out)   OUT_DIR="$2"; shift 2 ;;
        --name)  BUNDLE_NAME="$2"; shift 2 ;;
        --zip)   MAKE_ZIP=1; shift ;;
        -h|--help)
            sed -n '2,16p' "$0"
            exit 0
            ;;
        *)
            echo "unknown option: $1" >&2
            exit 1
            ;;
    esac
done

BUNDLE_DIR="${OUT_DIR}/${BUNDLE_NAME}"

echo "==> memstroy-inator client packager"
echo "    workspace : ${ROOT_DIR}"
echo "    bundle    : ${BUNDLE_DIR}"

# ─── Build release binaries ──────────────────────────────────────────
echo "==> cargo build --release"
cargo build --release \
    -p memstroy-gui \
    -p memstroy-assets-server \
    -p memstroy-cli

# ─── Stage the bundle ────────────────────────────────────────────────
echo "==> staging bundle"
rm -rf "${BUNDLE_DIR}"
mkdir -p "${BUNDLE_DIR}/bin"

# Binary names are stable between platforms (no .exe on unix).
for bin in memstroy-gui memstroy-assets-server memstroy; do
    src="target/release/${bin}"
    if [[ ! -x "${src}" ]]; then
        echo "missing release binary: ${src}" >&2
        exit 1
    fi
    cp -p "${src}" "${BUNDLE_DIR}/bin/"
done

# ─── Asset skeleton ──────────────────────────────────────────────────
# The editor expects an `assets/` tree relative to its launch dir. We
# copy the existing directory (which includes README sidecars) and
# pre-create the kind subdirectories the assets-server walks on
# startup.
echo "==> mirroring asset skeleton"
mkdir -p "${BUNDLE_DIR}/assets"
for sub in clips videos images sounds particles text mellstroy; do
    mkdir -p "${BUNDLE_DIR}/assets/${sub}"
done
# Copy any README.md sidecars from the source assets dir so the user
# sees the "drop files here" hints inside the bundle too.
if [[ -d "assets" ]]; then
    find assets -maxdepth 2 -name 'README.md' -print0 \
        | while IFS= read -r -d '' f; do
            target="${BUNDLE_DIR}/${f}"
            mkdir -p "$(dirname "${target}")"
            cp -p "${f}" "${target}"
        done
fi

# ─── Examples + docs ─────────────────────────────────────────────────
mkdir -p "${BUNDLE_DIR}/examples"
cp -p examples/*.yaml "${BUNDLE_DIR}/examples/" 2>/dev/null || true
cp -p README.md "${BUNDLE_DIR}/" 2>/dev/null || true
cp -p AI_MEME_INSTRUCTIONS.md "${BUNDLE_DIR}/" 2>/dev/null || true

# ─── Top-level launcher ──────────────────────────────────────────────
cat > "${BUNDLE_DIR}/memstroy-inator.sh" <<'LAUNCH'
#!/usr/bin/env bash
# Launch the editor from the bundle root so `assets/` is auto-discovered.
set -e
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "${SCRIPT_DIR}"
exec ./bin/memstroy-gui "$@"
LAUNCH
chmod +x "${BUNDLE_DIR}/memstroy-inator.sh"

# ─── Optional zip ────────────────────────────────────────────────────
if [[ "${MAKE_ZIP}" -eq 1 ]]; then
    echo "==> zipping bundle"
    (cd "${OUT_DIR}" && zip -qr "${BUNDLE_NAME}.zip" "${BUNDLE_NAME}")
    echo "    archive : ${OUT_DIR}/${BUNDLE_NAME}.zip"
fi

echo "==> done"
echo "    run with: ${BUNDLE_DIR}/memstroy-inator.sh"
