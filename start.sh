#!/bin/bash
# QuantKit Web API 服务 - 启动脚本
# 用法: ./start.sh
# 环境变量:
#   QUANTKIT_API_TOKEN  受保护接口令牌（必须设置，否则回测/下载/启停一律拒绝）
#   QUANTKIT_PORT       监听端口，默认 8080
#   QUANTKIT_DATA_DIR   K线/状态数据目录，默认 ./data
#   QUANTKIT_CONFIG     配置文件路径（可选）

set -e
set -o pipefail

# 非交互式 SSH 部署时 PATH 可能不含 cargo，显式加载
if [ -f "$HOME/.cargo/env" ]; then
    . "$HOME/.cargo/env"
fi

PROJECT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$PROJECT_DIR"

PID_FILE="$PROJECT_DIR/.quantkit-web.pid"
LOG_DIR="$PROJECT_DIR/logs"
PORT="${QUANTKIT_PORT:-8080}"
DATA_DIR="${QUANTKIT_DATA_DIR:-$PROJECT_DIR/data}"

# 检查是否已在运行
if [ -f "$PID_FILE" ]; then
    OLD_PID=$(cat "$PID_FILE")
    if kill -0 "$OLD_PID" 2>/dev/null; then
        echo "quantkit-web 已在运行 (PID: $OLD_PID)"
        exit 1
    else
        rm -f "$PID_FILE"
    fi
fi

# 令牌检查：未设置时受保护接口会 fail-closed，明确提示但不阻止启动（公开行情仍可用）
if [ -z "$QUANTKIT_API_TOKEN" ]; then
    echo "警告: 未设置 QUANTKIT_API_TOKEN，回测/下载/启停等受保护接口将全部拒绝"
fi

mkdir -p "$LOG_DIR" "$DATA_DIR"

# 编译 release 版本（quantkit CLI + quantkit-web 两个二进制）
# pipefail 保证 cargo 失败不会被 tail 掩盖，防止带着旧二进制启动
echo "编译中..."
if ! cargo build --release --bin quantkit --bin quantkit-web 2>&1 | tail -3; then
    echo "编译失败，已中止启动"
    exit 1
fi

# 后台启动 Web API 服务
echo "启动 quantkit-web (端口 $PORT, 数据目录 $DATA_DIR)..."
ARGS="--port $PORT --data $DATA_DIR"
if [ -n "$QUANTKIT_CONFIG" ]; then
    ARGS="$ARGS --config $QUANTKIT_CONFIG"
fi
nohup ./target/release/quantkit-web $ARGS > "$LOG_DIR/web.log" 2>&1 &
PID=$!

echo "$PID" > "$PID_FILE"
sleep 1

# 验证进程是否存活
if kill -0 "$PID" 2>/dev/null; then
    echo "quantkit-web 已启动 (PID: $PID)"
    echo "日志: $LOG_DIR/web.log"
    echo "停止: ./stop.sh"
else
    echo "启动失败，查看日志: $LOG_DIR/web.log"
    rm -f "$PID_FILE"
    exit 1
fi
