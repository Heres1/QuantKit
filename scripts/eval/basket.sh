#!/bin/bash
# M3: 等权篮子构建 — 每品种用各自 walkforward 众数参数独立跑自己的资金切片，曲线加总
cd ~/quantkit
B=./target/release/quantkit
OUT=/tmp/multi
# 众数参数（来自 M2 各折选择）: BTC 50/0.12  ETH 50/0.2  TRX 50/0.25  DOGE 50/0.12  BNB 50/0.2
$B backtest --strategy trend --symbols BTCUSDT  --ma-days 50 --trailing-stop 0.12 --cash 2000 --json 2>/dev/null > $OUT/eq_BTC.json
$B backtest --strategy trend --symbols ETHUSDT  --ma-days 50 --trailing-stop 0.2  --cash 2000 --json 2>/dev/null > $OUT/eq_ETH.json
$B backtest --strategy trend --symbols TRXUSDT  --ma-days 50 --trailing-stop 0.25 --cash 2000 --json 2>/dev/null > $OUT/eq_TRX.json
$B backtest --strategy trend --symbols DOGEUSDT --ma-days 50 --trailing-stop 0.12 --cash 2000 --json 2>/dev/null > $OUT/eq_DOGE.json
$B backtest --strategy trend --symbols BNBUSDT  --ma-days 50 --trailing-stop 0.2  --cash 2000 --json 2>/dev/null > $OUT/eq_BNB.json
echo "backtests done"
python3 - <<'EOF'
import json, itertools
from datetime import datetime, timezone

syms = ["BTC","ETH","TRX","DOGE","BNB"]
cash = 2000.0
curves = {}
first_ts = {}
for s in syms:
    d = json.load(open(f"/tmp/multi/eq_{s}.json"))
    c = d["equity_curve"]
    m = {p["timestamp"]: p["equity"] for p in c}
    curves[s] = m
    first_ts[s] = min(m)

all_ts = sorted(set().union(*[set(m) for m in curves.values()]))

def basket(members, ts_list):
    out = []
    for ts in ts_list:
        v = 0.0
        for s in members:
            m = curves[s]
            if ts < first_ts[s]:
                v += cash          # 未上市：现金闲置
            else:
                v += m.get(ts, cash)
        out.append((ts, v))
    return out

def report(name, curve, initial):
    tot = curve[-1][1]/initial - 1
    days = (curve[-1][0]-curve[0][0])/86400000
    ann = (1+tot)**(365/max(days,1)) - 1
    peak = curve[0][1]; mdd = 0.0
    for _, e in curve:
        peak = max(peak, e); mdd = max(mdd, (peak-e)/peak)
    print(f"{name}: total%={tot*100:+.1f} annual%={ann*100:+.1f} maxDD%={mdd*100:.1f} ({days/365:.1f}y)")
    # 分年度
    yr = {}
    for ts, e in curve:
        y = datetime.fromtimestamp(ts/1000, timezone.utc).year
        yr.setdefault(y, []).append(e)
    prev = None
    row = []
    for y in sorted(yr):
        s = prev if prev else initial
        row.append("%d:%+.1f%%" % (y, (yr[y][-1]/s-1)*100))
        prev = yr[y][-1]
    print("   " + " ".join(row))

I3 = 3*cash; I4 = 4*cash; I5 = 5*cash
b1 = basket(["BTC","TRX","DOGE"], all_ts)
b2 = basket(["BTC","ETH","TRX","DOGE"], all_ts)
b3 = basket(["BTC","ETH","TRX","DOGE","BNB"], all_ts)
btc_only = basket(["BTC"], all_ts)
report("B1 严格篮 BTC+TRX+DOGE      ", b1, I3)
report("B2 含ETH                  ", b2, I4)
report("B3 含ETH+BNB              ", b3, I5)
report("对照 BTC单币趋势(全资金)   ", [(ts, e * 5) for ts, e in btc_only], 10000)

# 公共窗口对比（最晚品种上市后，全员齐全段）
common = [ts for ts in all_ts if ts >= max(first_ts.values())]
cstart = datetime.fromtimestamp(common[0]/1000, timezone.utc).strftime("%Y-%m")
report(f"B1 公共窗口({cstart}起齐全)",
       [(ts, sum(curves[s][ts] for s in ["BTC","TRX","DOGE"])) for ts in common], I3)

# 日收益相关性（重叠期，基于各自完整曲线）
def rets(s):
    m = curves[s]; ts = sorted(m)
    return {ts[i]: m[ts[i]]/m[ts[i-1]]-1 for i in range(1, len(ts))}
r = {s: rets(s) for s in syms}
print("\n日收益相关系数（重叠日）:")
for a, b in itertools.combinations(syms, 2):
    common_k = set(r[a]) & set(r[b])
    if len(common_k) < 100: continue
    xa = [r[a][k] for k in common_k]; xb = [r[b][k] for k in common_k]
    ma = sum(xa)/len(xa); mb = sum(xb)/len(xb)
    cov = sum((x-ma)*(y-mb) for x, y in zip(xa, xb))
    va = sum((x-ma)**2 for x in xa)**0.5; vb = sum((y-mb)**2 for y in xb)**0.5
    print(f"  {a:>4}-{b:<4} {cov/(va*vb):+.2f}  ({len(common_k)}日)")
EOF
echo "BASKET DONE"
