#!/bin/bash
# QuantKit 统一部署脚本
# 用途: 把最新代码同步到服务器，编译并重启 quantkit-web
# 用法:
#   ./scripts/deploy.sh           # 同步代码 + 编译 + 重启后端
#   ./scripts/deploy.sh --live    # 同上，并在重启后启动实盘
#
# 前提: 已配置 SSH 密钥免密登录（见 scripts/setup-remote.sh）

set -e
set -o pipefail

SERVER_USER="${QUANTKIT_SERVER_USER:-ubuntu}"
SERVER_HOST="${QUANTKIT_SERVER_HOST:-43.154.120.27}"
SERVER="$SERVER_USER@$SERVER_HOST"
REMOTE_DIR="~/quantkit"
WEB_PORT="${QUANTKIT_WEB_PORT:-8080}"
START_LIVE=false

if [ "$1" = "--live" ]; then
    START_LIVE=true
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
info "重启 quantkit-web (端口 $WEB_PORT)..."
ssh "$SERVER" << EOF
set -e
source \$HOME/.cargo/env 2>/dev/null
cd $REMOTE_DIR
if [ -f .env.server ]; then . ./.env.server; fi
pkill -f quantkit-web || true
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

# 6. 可选: 启动实盘
if [ "$START_LIVE" = true ]; then
    info "启动实盘..."
    TOKEN=$(ssh "$SERVER" 'cd ~/quantkit && { . ./.env.server 2>/dev/null; echo "${QUANTKIT_API_TOKEN:-}"; }')
    if [ -z "$TOKEN" ]; then
        error "服务器未设置 QUANTKIT_API_TOKEN（写入 ~/quantkit/.env.server），无法启动实盘"
        exit 1
    fi
    RESP=$(ssh "$SERVER" "curl -s -X POST -H 'X-API-Token: $TOKEN' http://localhost:$WEB_PORT/api/runs/live/start")
    if echo "$RESP" | grep -q '"ok":true'; then
        info "✅ 实盘已启动: $RESP"
    else
        error "实盘启动失败: $RESP"
        exit 1
    fi
fi

echo ""
info "部署完成"
echo "  检查状态: ./scripts/check-live.sh"
echo "  查看日志: ssh $SERVER 'tail -f /tmp/quantkit-web.log'"
