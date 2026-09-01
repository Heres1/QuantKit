import json, sys
d = json.load(sys.stdin)
ec = d["equity_curve"]
peak = 0.0; mdd = 0.0
for p in ec:
    e = p["equity"]; peak = max(peak, e)
    if peak > 0: mdd = max(mdd, (peak - e) / peak)
fin = ec[-1]["equity"]
yrs = (ec[-1]["timestamp"] - ec[0]["timestamp"]) / 86400000 / 365.25
total = 100 * (fin / 10000 - 1)
annual = (fin / 10000) ** (1 / yrs) * 100 - 100
n = len(d.get("trades", []))
print(f"final_equity={fin:.0f} total={total:.1f}% years={yrs:.1f} annual={annual:.1f}% maxDD={mdd*100:.1f}% trades={n}")
