#!/bin/bash
# 多品种趋势评估：M1 固定参数全历史筛查 + M2 逐品种 walkforward（防过拟合）
cd ~/quantkit
B=./target/release/quantkit
OUT=/tmp/multi; mkdir -p $OUT
LONG="BTCUSDT ETHUSDT BNBUSDT LTCUSDT XRPUSDT ADAUSDT TRXUSDT XLMUSDT VETUSDT"
MID="SOLUSDT LINKUSDT DOGEUSDT DOTUSDT ATOMUSDT AVAXUSDT UNIUSDT FILUSDT"
ALL="$LONG $MID"

MET='import json,sys
m=json.load(sys.stdin)["metrics"]
print("total%%=%.1f annual%%=%.1f maxDD%%=%.1f sharpe=%.2f calmar=%.2f trades=%d win%%=%.1f" % (
 m["total_return_pct"],m["annualized_return_pct"],m["max_drawdown_pct"],
 m["sharpe_ratio"],(m["calmar_ratio"] or 0),m["num_round_trips"],m["win_rate_pct"]))'

echo "===== [M1] 固定参数 ma50/trail0.12 全历史趋势（无逐品种调参） ====="
for S in $ALL; do
  r=$($B backtest --strategy trend --symbols $S --json 2>/dev/null | python3 -c "$MET")
  echo "$S $r"
done

echo "--- 基准：买入持有 ---"
python3 - <<'EOF'
import json, glob
for f in sorted(glob.glob("/home/ubuntu/binance-rust/data/history/*_1d.json")):
    sym=f.split("/")[-1].replace("_1d.json","")
    if sym in ("USDCUSDT","MATICUSDT"): continue
    d=json.load(open(f))
    o,c=d[0]["open"],d[-1]["close"]
    days=(d[-1]["close_time"]-d[0]["open_time"])/86400000
    tot=c/o-1; ann=(1+tot)**(365/days)-1
    peak=d[0]["close"];mdd=0
    for b in d:
        peak=max(peak,b["close"]);mdd=max(mdd,(peak-b["close"])/peak)
    print("%s hold: total%%=%.1f annual%%=%.1f maxDD%%=%.1f (%.1fy)" % (sym,tot*100,ann*100,mdd*100,days/365))
EOF

echo "===== [M2] 逐品种 walkforward 4折: ma{50,100,200} x trail{0.12,0.20,0.25} ====="
for S in $ALL; do
  echo "--- $S ---"
  $B walkforward --strategy trend --symbols $S --grid "ma_days=50,100,200;trailing_stop=0.12,0.20,0.25" --windows 4 --rank sharpe 2>/dev/null | tee $OUT/wf_$S.txt | tail -30
done
echo "===== DONE ====="
