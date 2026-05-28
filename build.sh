#!/usr/bin/env bash
# 一键构建: admin-ui (pnpm build) + xkiro-rs (cargo build --release)
# 用法:
#   ./build.sh             # ui + release
#   ./build.sh debug       # ui + cargo build (dev)
#   ./build.sh ui          # 只 ui
#   ./build.sh rs          # 只 cargo (release)
#   ./build.sh rs debug    # 只 cargo (dev)
#   SKIP_UI=1 ./build.sh   # 跳过 ui
#   SKIP_RS=1 ./build.sh   # 跳过 cargo

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
UI_DIR="$SCRIPT_DIR/admin-ui"

GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

log()  { echo -e "${GREEN}[build]${NC} $*"; }
warn() { echo -e "${YELLOW}[build]${NC} $*"; }
err()  { echo -e "${RED}[build]${NC} $*" >&2; }

mode="release"
target="all"

case "${1:-}" in
  ui)      target="ui" ;;
  rs)      target="rs"; mode="${2:-release}" ;;
  debug)   mode="debug" ;;
  release) mode="release" ;;
  "")      ;;
  *)       err "未知参数: $1"; exit 2 ;;
esac

build_ui() {
  if [[ "${SKIP_UI:-0}" == "1" ]]; then
    warn "SKIP_UI=1, 跳过 admin-ui"
    return
  fi
  if [[ ! -d "$UI_DIR" ]]; then
    err "找不到 $UI_DIR"; exit 1
  fi
  log "admin-ui: pnpm build"
  ( cd "$UI_DIR" && pnpm build )
}

build_rs() {
  if [[ "${SKIP_RS:-0}" == "1" ]]; then
    warn "SKIP_RS=1, 跳过 cargo"
    return
  fi
  if [[ "$mode" == "release" ]]; then
    log "cargo build --release"
    ( cd "$SCRIPT_DIR" && cargo build --release )
    log "产物: $SCRIPT_DIR/target/release/xkiro-rs"
  else
    log "cargo build (dev)"
    ( cd "$SCRIPT_DIR" && cargo build )
    log "产物: $SCRIPT_DIR/target/debug/xkiro-rs"
  fi
}

start=$(date +%s)
case "$target" in
  ui)  build_ui ;;
  rs)  build_rs ;;
  all) build_ui; build_rs ;;
esac
end=$(date +%s)
log "完成, 耗时 $((end - start))s"
