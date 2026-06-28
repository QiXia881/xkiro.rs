#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEBUG_DIR="$SCRIPT_DIR/target/debug"
BIN="$DEBUG_DIR/xkiro-rs"
CONFIG="$DEBUG_DIR/config.json"
CREDENTIALS="$DEBUG_DIR/credentials.json"
UI_DIR="$SCRIPT_DIR/admin-ui"

read_bind_addr() {
  if command -v python3 >/dev/null 2>&1; then
    python3 - "$CONFIG" <<'PY'
import json
import sys

host = "127.0.0.1"
port = 8080

try:
    with open(sys.argv[1], encoding="utf-8") as f:
        config = json.load(f)
    host = str(config.get("host") or host)
    port = int(config.get("port") or port)
except Exception:
    pass

print(f"{host} {port}")
PY
    return
  fi

  echo "127.0.0.1 8080"
}

is_port_open() {
  local host="$1"
  local port="$2"

  if [[ "$host" == "0.0.0.0" || "$host" == "::" ]]; then
    host="127.0.0.1"
  fi

  if command -v python3 >/dev/null 2>&1; then
    python3 - "$host" "$port" <<'PY'
import socket
import sys

try:
    with socket.create_connection((sys.argv[1], int(sys.argv[2])), timeout=0.2):
        pass
except OSError:
    sys.exit(1)
PY
    return
  fi

  (echo >/dev/tcp/"$host"/"$port") >/dev/null 2>&1
}

print_port_owner() {
  local port="$1"

  if command -v lsof >/dev/null 2>&1; then
    lsof -nP -iTCP:"$port" -sTCP:LISTEN 2>/dev/null | awk 'NR == 1 || NR <= 8'
    return
  fi

  if command -v ss >/dev/null 2>&1; then
    ss -ltnp "sport = :$port" 2>/dev/null
    return
  fi

  if command -v fuser >/dev/null 2>&1; then
    fuser -v -n tcp "$port" 2>/dev/null
    return
  fi

  return 1
}

list_port_pids() {
  local port="$1"

  if command -v lsof >/dev/null 2>&1; then
    lsof -tiTCP:"$port" -sTCP:LISTEN 2>/dev/null | sort -u
    return
  fi

  if command -v fuser >/dev/null 2>&1; then
    fuser -n tcp "$port" 2>/dev/null | tr ' ' '\n' | sed '/^$/d' | sort -u
    return
  fi

  if command -v ss >/dev/null 2>&1; then
    ss -ltnp "sport = :$port" 2>/dev/null | sed -n 's/.*pid=\([0-9][0-9]*\).*/\1/p' | sort -u
    return
  fi
}

is_backend_pid() {
  local pid="$1"
  local exe=""
  local exe_path=""
  local bin_path=""
  local cmdline=""

  exe="$(readlink "/proc/$pid/exe" 2>/dev/null || true)"
  exe="${exe% (deleted)}"
  if [[ -n "$exe" ]]; then
    exe_path="$(realpath "$exe" 2>/dev/null || printf '%s\n' "$exe")"
    bin_path="$(realpath "$BIN" 2>/dev/null || printf '%s\n' "$BIN")"
    if [[ "$exe_path" == "$bin_path" || "$(basename "$exe_path")" == "xkiro-rs" ]]; then
      return 0
    fi
  fi

  cmdline="$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null || true)"
  [[ "$cmdline" == *"$BIN"* || "$cmdline" == *"target/debug/xkiro-rs"* ]]
}

stop_old_backend_on_port() {
  local host="$1"
  local port="$2"
  local pid=""
  local -a pids=()
  local -a backend_pids=()

  mapfile -t pids < <(list_port_pids "$port")
  for pid in "${pids[@]}"; do
    [[ "$pid" =~ ^[0-9]+$ ]] || continue
    if is_backend_pid "$pid"; then
      backend_pids+=("$pid")
    fi
  done

  if [[ "${#backend_pids[@]}" -eq 0 ]]; then
    return 1
  fi

  echo "检测到旧后端进程，正在停止: ${backend_pids[*]}" >&2
  kill "${backend_pids[@]}" 2>/dev/null || true

  for _ in {1..30}; do
    if ! is_port_open "$host" "$port"; then
      return 0
    fi
    sleep 0.1
  done

  echo "旧后端未及时释放端口，正在强制停止: ${backend_pids[*]}" >&2
  kill -9 "${backend_pids[@]}" 2>/dev/null || true

  for _ in {1..30}; do
    if ! is_port_open "$host" "$port"; then
      return 0
    fi
    sleep 0.1
  done

  return 1
}

admin_api_enabled() {
  if command -v python3 >/dev/null 2>&1; then
    python3 - "$CONFIG" <<'PY'
import json
import sys

try:
    with open(sys.argv[1], encoding="utf-8") as f:
        admin_key = json.load(f).get("adminApiKey")
except Exception:
    sys.exit(1)

if isinstance(admin_key, str) and admin_key.strip():
    sys.exit(0)

sys.exit(1)
PY
    return
  fi

  grep -Eq '"adminApiKey"[[:space:]]*:[[:space:]]*"[^"]+"' "$CONFIG"
}

if [[ "${DEV_SKIP_BUILD:-0}" != "1" ]]; then
  (cd "$SCRIPT_DIR" && cargo build)
fi

case "${1:-}" in
  init|social-helper|help|-h|--help|-V|--version)
    exec "$BIN" -c "$CONFIG" --credentials "$CREDENTIALS" "$@"
    ;;
esac

if [[ ! -f "$CONFIG" ]]; then
  "$BIN" -c "$CONFIG" --credentials "$CREDENTIALS" init
  if [[ ! -f "$CONFIG" ]]; then
    echo "配置文件未生成: $CONFIG" >&2
    exit 1
  fi
fi

if ! admin_api_enabled; then
  echo "Admin API 未启用: $CONFIG 中的 adminApiKey 为空或未配置。" >&2
  echo "dev.sh 会启动 Admin UI，必须配置 adminApiKey；否则登录后 /api/admin/* 会返回 404。" >&2
  echo "处理方式: 编辑 $CONFIG 设置 adminApiKey，或运行 ./dev.sh init --force 并在向导里填写 adminApiKey。" >&2
  exit 1
fi

read -r BACKEND_HOST BACKEND_PORT < <(read_bind_addr)
FRONTEND_BACKEND_HOST="$BACKEND_HOST"
if [[ "$FRONTEND_BACKEND_HOST" == "0.0.0.0" || "$FRONTEND_BACKEND_HOST" == "::" ]]; then
  FRONTEND_BACKEND_HOST="127.0.0.1"
fi

if is_port_open "$BACKEND_HOST" "$BACKEND_PORT"; then
  if stop_old_backend_on_port "$BACKEND_HOST" "$BACKEND_PORT"; then
    echo "旧后端已停止，继续启动。" >&2
  else
    echo "后端端口已被占用: $BACKEND_HOST:$BACKEND_PORT" >&2
    echo "占用进程:" >&2
    if ! print_port_owner "$BACKEND_PORT" >&2; then
      echo "  未能自动识别占用进程；可手动运行: lsof -nP -iTCP:$BACKEND_PORT -sTCP:LISTEN" >&2
    fi
    echo "占用进程不是 $BIN，dev.sh 不会自动结束它。" >&2
    echo "请先停止占用该端口的进程后再运行 ./dev.sh。" >&2
    exit 1
  fi
fi

cleanup() {
  trap - EXIT INT TERM
  if [[ -n "${BACKEND_PID:-}" ]]; then
    kill "$BACKEND_PID" 2>/dev/null || true
    wait "$BACKEND_PID" 2>/dev/null || true
  fi
  if [[ -n "${FRONTEND_PID:-}" ]]; then
    kill "$FRONTEND_PID" 2>/dev/null || true
    wait "$FRONTEND_PID" 2>/dev/null || true
  fi
}

trap cleanup EXIT INT TERM

"$BIN" -c "$CONFIG" --credentials "$CREDENTIALS" "$@" &
BACKEND_PID=$!

sleep 0.5
if ! kill -0 "$BACKEND_PID" 2>/dev/null; then
  wait "$BACKEND_PID"
  exit 1
fi

(cd "$UI_DIR" && XKIRO_BACKEND_URL="http://$FRONTEND_BACKEND_HOST:$BACKEND_PORT" pnpm dev) &
FRONTEND_PID=$!

wait -n "$BACKEND_PID" "$FRONTEND_PID"
