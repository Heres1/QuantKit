#!/bin/bash
# QuantKit 统一部署脚本
# 用途: 把最新代码同步到服务器，编译并重启 quantkit-web，然后重启实盘接管原有状态
# 用法:
#   ./scripts/deploy.sh            # 同步代码 + 编译 + 重启后端 + 重启实盘
#   ./scripts/deploy.sh --no-live  # 同上，但不启动实盘（仅在你刻意要停盘时用）
#
# 实盘状态接管: 持仓/成交/权益存于服务器 live_quantkit_state.json，
# 重启后自动恢复并与交易所对账；调仓周期/止损峰值由历史重放确定性重建，无需人工干预。
#
# 前提: 已配置 SSH 密钥免密登录（一次性操作: ssh-copy-id ubuntu@43.154.120.27）

set -e
set -o pipefail

SERVER_USER="${QUANTKIT_SERVER_USER:-ubuntu}"
SERVER_HOST="${QUANTKIT_SERVER_HOST:-43.154.120.27}"
SERVER="$SERVER_USER@$SERVER_HOST"
REMOTE_DIR="~/quantkit"
WEB_PORT="${QUANTKIT_WEB_PORT:-8080}"
START_LIVE=true

if [ "$1" = "--no-live" ]; then
    START_LIVE=false
fi

GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
NC='\033[0m'

info() { echo -e "${GREEN}[INFO]${NC} $1"; }
warn() { echo -e "${YELLOW}[WARN]${NC} $1"; }
error() { echo -e "${RED}[ERROR]${NC} $1"; }

# 1. 连接检查
info "检查 SSH 连接到 $SERVER ..."
if ! ssh -o BatchMode=yes -o ConnectTimeout=5 "$SERVER" "echo ok" &>/dev/null; then
    error "无法免密连接 $SERVER"
    error "请先配置 SSH 密钥: ssh-copy-id $SERVER"
    exit 1
fi

# 2. 同步代码（用 git：若已配置 server 远程仓库则 push，否则用 rsync 兜底）
info "同步代码到服务器..."
if git remote get-url server &>/dev/null; then
    BRANCH=$(git branch --show-current)
    # bare 仓库的 post-receive 钩子会自动把 ~/quantkit 同步到最新
    git push server "$BRANCH"
else
    warn "未配置指向服务器的远程仓库，使用 rsync 同步"
    rsync -az --delete \
        --exclude target --exclude .git --exclude node_modules \
        --exclude frontend/node_modules --exclude frontend/dist \
        --exclude data --exclude logs --exclude quantkit.toml \
        --exclude '*.pid' --exclude 'live_*.json' \
        ./ "$SERVER:$REMOTE_DIR/"
fi

# 3. 编译
info "在服务器上编译 release（可能需要几分钟）..."
ssh "$SERVER" "source \$HOME/.cargo/env 2>/dev/null; cd $REMOTE_DIR && cargo build --release --bin quantkit --bin quantkit-web 2>&1 | tail -5"

# 4. 重启 quantkit-web（保留服务器已有的 QUANTKIT_API_TOKEN 环境变量文件）
# 先杀实盘子进程再杀 web：实盘由 web 以 `quantkit live` 子进程方式托管，
# 只杀 web 会让实盘变成孤儿进程继续下单，重启后再启动就出现双实盘。
# [q] 括号写法避免 pkill 的模式字符串匹配到 ssh 会话自身的命令行。
info "重启 quantkit-web (端口 $WEB_PORT)..."
ssh "$SERVER" << EOF
set -e
source \$HOME/.cargo/env 2>/dev/null
cd $REMOTE_DIR
if [ -f .env.server ]; then . ./.env.server; fi
pkill -f "[q]uantkit live" || true
pkill -f "[q]uantkit dry-run" || true
pkill -f "[q]uantkit-web" || true
sleep 2
mkdir -p logs
nohup ./target/release/quantkit-web --port $WEB_PORT > /tmp/quantkit-web.log 2>&1 &
sleep 3
EOF

# 5. 健康检查
info "验证服务..."
if ssh "$SERVER" "curl -s -m 5 http://localhost:$WEB_PORT/api/health" | grep -q '"ok":true'; then
    info "✅ quantkit-web 运行正常 (端口 $WEB_PORT)"
else
    error "服务未通过健康检查，查看日志: ssh $SERVER 'tail /tmp/quantkit-web.log'"
    exit 1
fi

# 6. 重启运行进程（默认行为）：按服务器 quantkit.toml 的 live_enabled 决定
#    true  -> 启动实盘（从状态文件接管原持仓，与交易所对账后继续运行）
#    false -> 启动纸上交易 dryrun（真金不参与，影子账本独立于实盘状态文件）
if [ "$START_LIVE" = true ]; then
    TOKEN=$(ssh "$SERVER" 'cd ~/quantkit && { . ./.env.server 2>/dev/null; echo "${QUANTKIT_API_TOKEN:-}"; }')
    if [ -z "$TOKEN" ]; then
        error "服务器未设置 QUANTKIT_API_TOKEN（写入 ~/quantkit/.env.server），无法启动运行进程"
        exit 1
    fi
    LIVE_ENABLED=$(ssh "$SERVER" "grep -E '^live_enabled[[:space:]]*=' $REMOTE_DIR/quantkit.toml | head -1 | grep -c true" || true)
    if [ "$LIVE_ENABLED" = "1" ]; then
        info "启动实盘（自动接管重启前的持仓与成交记录）..."
        RESP=$(ssh "$SERVER" "curl -s -X POST -H 'X-API-Token: $TOKEN' http://localhost:$WEB_PORT/api/runs/live/start")
        if ! echo "$RESP" | grep -q '"ok":true'; then
            error "实盘启动失败: $RESP"
            exit 1
        fi
        info "✅ 实盘已启动: $RESP"

        # 验证状态接管：等待自检+对账完成，检查日志中的恢复记录
        sleep 8
        LOGS=$(ssh "$SERVER" "curl -s -H 'X-API-Token: $TOKEN' http://localhost:$WEB_PORT/api/runs/live/logs")
        if echo "$LOGS" | grep -q "恢复状态"; then
            info "✅ 状态接管成功: $(echo "$LOGS" | grep -o '恢复状态[^"]*' | head -1)"
        elif echo "$LOGS" | grep -q "自检"; then
            warn "实盘已进入自检，未检测到历史状态（可能是首次启动，无历史可接管）"
        else
            warn "暂未读到实盘日志，请手动确认: ./scripts/check-live.sh"
        fi
    else
        info "live_enabled=false，启动纸上交易 (dryrun)..."
        RESP=$(ssh "$SERVER" "curl -s -X POST -H 'X-API-Token: $TOKEN' http://localhost:$WEB_PORT/api/runs/dryrun/start")
        if ! echo "$RESP" | grep -q '"ok":true'; then
            error "纸上交易启动失败: $RESP"
            exit 1
        fi
        info "✅ 纸上交易已启动: $RESP"
    fi
else
    warn "已跳过运行进程启动（--no-live）"
fi

echo ""
info "部署完成"
echo "  检查状态: ./scripts/check-live.sh"
echo "  查看日志: ssh $SERVER 'tail -f /tmp/quantkit-web.log'"
