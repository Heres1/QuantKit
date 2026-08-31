#!/bin/bash
# QuantKit 实盘状态快速检查脚本
# 用法: QUANTKIT_API_TOKEN=<令牌> ./scripts/check-live.sh
# 令牌不设默认值，避免密钥入库；服务器令牌存放于 ~/quantkit/.env.server

SERVER="ubuntu@43.154.120.27"
API_BASE="http://43.154.120.27:8080"
TOKEN="${QUANTKIT_API_TOKEN:-}"

if [ -z "$TOKEN" ]; then
    # 尝试从服务器 .env.server 读取，免去手动传参
    TOKEN=$(ssh "$SERVER" 'cd ~/quantkit && . ./.env.server 2>/dev/null && echo "$QUANTKIT_API_TOKEN"' 2>/dev/null)
fi

echo "=========================================="
echo "  QuantKit 实盘状态检查"
echo "  $(date '+%Y-%m-%d %H:%M:%S')"
echo "=========================================="
echo ""

# 1. 进程状态（quantkit-web 承载实盘）
echo "1. 进程状态："
PROCESS_INFO=$(ssh "$SERVER" "ps aux | grep quantkit-web | grep -v grep" 2>/dev/null)
if [ -n "$PROCESS_INFO" ]; then
    PID=$(echo "$PROCESS_INFO" | awk '{print $2}')
    STARTED=$(echo "$PROCESS_INFO" | awk '{print $9}')
    echo "   ✅ quantkit-web 运行中 (PID: $PID, 启动: $STARTED)"
else
    echo "   ❌ quantkit-web 未运行"
    echo "   启动: ./scripts/deploy.sh"
fi
echo ""

# 2. 健康检查
echo "2. API 健康检查："
HEALTH=$(curl -s -m 5 "$API_BASE/api/health" 2>/dev/null)
if echo "$HEALTH" | grep -q '"ok":true'; then
    echo "   ✅ $HEALTH"
else
    echo "   ❌ 无法访问 $API_BASE/api/health"
fi
echo ""

# 3. 实盘账户（需令牌）
echo "3. 实盘账户（/api/live/account）："
ACCOUNT=$(curl -s -m 5 -H "X-API-Token: $TOKEN" "$API_BASE/api/live/account" 2>/dev/null)
if echo "$ACCOUNT" | grep -q '"ok":true'; then
    echo "$ACCOUNT" | python3 -c "
import sys, json
d = json.load(sys.stdin)['data']
print(f\"   总资产: {d.get('total_equity', 'N/A')} USDT\")
print(f\"   可用余额: {d.get('available', 'N/A')} USDT\")
print(f\"   持仓数: {len(d.get('positions', []))}\")" 2>/dev/null || echo "   $ACCOUNT"
else
    echo "   ❌ $ACCOUNT"
    echo "   （实盘未启动时此接口报错属正常，先执行 /api/runs/live/start）"
fi
echo ""

# 4. 状态文件
echo "4. 状态文件："
STATE_INFO=$(ssh "$SERVER" "ls -lh ~/quantkit/live_quantkit_state.json 2>/dev/null" 2>/dev/null)
if [ -n "$STATE_INFO" ]; then
    echo "   ✅ $STATE_INFO"
else
    echo "   ❌ 状态文件不存在（实盘从未在此服务器启动过）"
fi
echo ""

# 5. 最新日志
echo "5. 最新日志（最后 10 行）："
ssh "$SERVER" "tail -10 /tmp/quantkit-web.log 2>/dev/null || tail -10 ~/quantkit/logs/web.log" 2>/dev/null | sed 's/^/   /'
echo ""

echo "=========================================="
echo "前端监控: 本地运行 frontend 后访问 实盘监控 页面"
echo "手动启动实盘:"
echo "  curl -X POST -H 'X-API-Token: \$TOKEN' $API_BASE/api/runs/live/start"
echo "=========================================="
