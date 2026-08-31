#!/bin/bash
# QuantKit Web API 服务 - 停止脚本

PROJECT_DIR="$(cd "$(dirname "$0")" && pwd)"
PID_FILE="$PROJECT_DIR/.quantkit-web.pid"

if [ ! -f "$PID_FILE" ]; then
    echo "quantkit-web 未在运行 (无PID文件)"
    exit 0
fi

PID=$(cat "$PID_FILE")

if kill -0 "$PID" 2>/dev/null; then
    echo "停止 quantkit-web (PID: $PID)..."
    kill "$PID"

    # 等待进程退出（最多10秒）
    for i in $(seq 1 10); do
        if ! kill -0 "$PID" 2>/dev/null; then
            break
        fi
        sleep 1
    done

    # 如果还没退出，强制杀掉
    if kill -0 "$PID" 2>/dev/null; then
        echo "进程未响应，强制终止..."
        kill -9 "$PID"
    fi

    echo "quantkit-web 已停止"
else
    echo "进程已不存在 (PID: $PID)"
fi

rm -f "$PID_FILE"
