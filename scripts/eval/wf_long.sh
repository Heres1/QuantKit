#!/bin/bash
# 长数据（2017~）策略验证：全历史基线 + 稳健性网格 + walkforward 防过拟合
cd ~/quantkit
B=./target/release/quantkit

MET='import json,sys
m=json.load(sys.stdin)["metrics"]
print("total%%=%.1f annual%%=%.1f maxDD%%=%.1f sharpe=%.2f calmar=%.2f trades=%d win%%=%.1f" % (
 m["total_return_pct"],m["annualized_return_pct"],m["max_drawdown_pct"],
 m["sharpe_ratio"],(m["calmar_ratio"] or 0),m["num_round_trips"],m["win_rate_pct"]))'

YEAR='import json,sys
r=json.load(sys.stdin)["equity_curve"]
from datetime import datetime,timezone
yr={}
for p in r:
    y=datetime.fromtimestamp(p["timestamp"]/1000,timezone.utc).year
    yr.setdefault(y,[]).append(p["equity"])
prev=None
for y in sorted(yr):
    eq=yr[y]
    s=prev if prev else eq[0]
    peak=eq[0];mdd=0.0
    for e in eq:
        peak=max(peak,e);mdd=max(mdd,(peak-e)/peak)
    print("  %d: %+.1f%%  (年内maxDD %.1f%%)" % (y,(eq[-1]/s-1)*100,mdd*100))
    prev=eq[-1]'

echo "===== [L1] 全历史(2017~) 现行参数 mom90/ma50/reb30/trail.12/top1 ====="
$B backtest --json 2>/dev/null | python3 -c "$MET"
echo "--- BTC/ETH 买入持有基准 ---"
python3 - <<'EOF'
import json,math
for sym in ("BTCUSDT","ETHUSDT"):
    d=json.load(open("/home/ubuntu/binance-rust/data/history/%s_1d.json"%sym))
    o,c=d[0]["open"],d[-1]["close"]
    days=(d[-1]["close_time"]-d[0]["open_time"])/86400000
    tot=c/o-1; ann=(1+tot)**(365/days)-1
    peak=d[0]["close"];mdd=0.0
    for b in d:
        peak=max(peak,b["close"]);mdd=max(mdd,(peak-b["close"])/peak)
    print("%s: total%%=%.1f annual%%=%.1f maxDD%%=%.1f (%d根 %.0f年)" % (sym,tot*100,ann*100,mdd*100,len(d),days/365))
EOF
echo "--- 分年度(策略连续曲线切分) ---"
$B backtest --json 2>/dev/null | python3 -c "$YEAR"

echo "===== [L2] 稳健性网格 momentum{60,90,120} x trailing{0.08,0.12,0.16} ====="
$B sweep --grid "momentum_days=60,90,120;trailing_stop=0.08,0.12,0.16" 2>/dev/null

echo "===== [L3] walkforward 4折: mom{90,120} x trail{0.12,0.16} ====="
$B walkforward --grid "momentum_days=90,120;trailing_stop=0.12,0.16" --windows 4 --rank sharpe 2>/dev/null

echo "===== [L4] walkforward 4折: regime_ma{0,200} x top_n{1,3} ====="
$B walkforward --grid "regime_ma=0,200;top_n=1,3" --windows 4 --rank sharpe 2>/dev/null

echo "===== [L5] walkforward 4折: mom{60,90,120} x top_n{1,3} ====="
$B walkforward --grid "momentum_days=60,90,120;top_n=1,3" --windows 4 --rank sharpe 2>/dev/null

echo "===== [L6] BTC趋势(全历史): sweep ma{50,100,200} x trail{0.12,0.20,0.25} ====="
$B sweep --strategy trend --symbols BTCUSDT --grid "ma_days=50,100,200;trailing_stop=0.12,0.20,0.25" 2>/dev/null

echo "===== [L7] walkforward 4折 BTC趋势: ma{50,100,200} x trail{0.12,0.20,0.25} ====="
$B walkforward --strategy trend --symbols BTCUSDT --grid "ma_days=50,100,200;trailing_stop=0.12,0.20,0.25" --windows 4 --rank sharpe 2>/dev/null

echo "===== DONE ====="
