#!/usr/bin/env bash
# scripts/start-server.sh
#
# Build (in release mode) and start the standalone memstroy-assets-server.
# When invoked from inside the workspace this is the same binary the GUI
# auto-spawns in-process — running it externally is useful when you want
# to share a single asset library across machines, or keep the server
# alive when the editor is closed.
#
# Usage:
#   scripts/start-server.sh                       # 0.0.0.0:8765, ./assets
#   scripts/start-server.sh --addr 127.0.0.1:9000 # custom bind
#   scripts/start-server.sh --root /var/lib/memstroy/assets
#
# Env knobs:
#   RUST_LOG  — tracing filter, default "info,memstroy_assets_server=info".
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${ROOT_DIR}"

ADDR="0.0.0.0:8765"
ASSET_ROOT="${ROOT_DIR}/assets"
PROFILE="release"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --addr)  ADDR="$2"; shift 2 ;;
        --root)  ASSET_ROOT="$2"; shift 2 ;;
        --debug) PROFILE="debug"; shift ;;
        -h|--help)
            sed -n '2,18p' "$0"
            exit 0
            ;;
        *)
            echo "unknown option: $1" >&2
            exit 1
            ;;
    esac
done

mkdir -p "${ASSET_ROOT}"
export RUST_LOG="${RUST_LOG:-info,memstroy_assets_server=info}"

echo "==> memstroy-assets-server"
echo "    addr   : ${ADDR}"
echo "    root   : ${ASSET_ROOT}"
echo "    profile: ${PROFILE}"

if [[ "${PROFILE}" == "release" ]]; then
    exec cargo run --release -p memstroy-assets-server -- \
        --addr "${ADDR}" --root "${ASSET_ROOT}"
else
    exec cargo run -p memstroy-assets-server -- \
        --addr "${ADDR}" --root "${ASSET_ROOT}"
fi
