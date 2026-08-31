#!/usr/bin/env python3
"""翻页拉取 Binance 长历史日K并就地覆盖 {SYM}_1d.json。

用法（在数据目录下执行或传目录参数）:
    python3 fetch_long_history.py [data_dir] [--start-ms 1483228800000]

行为:
- 品种自动取自目录下现存的 *_1d.json（保持与发现池一致）
- 从 --start-ms（默认 2017-01-01）向前翻页，limit=1000/页，
  实际起点由交易所上市时间决定
- 丢弃未收盘的当前 bar（close_time > now），与实盘"只用已收盘数据"一致
- 覆盖前先备份原文件到目录内 backup-<date>/
"""
import json
import os
import sys
import time
import urllib.request
from datetime import datetime, timezone

BASE = "https://api.binance.com/api/v3/klines"
DEFAULT_START_MS = 1483228800000  # 2017-01-01 00:00 UTC


def fetch_symbol(sym: str, start_ms: int) -> list:
    out = []
    cursor = start_ms
    while True:
        url = f"{BASE}?symbol={sym}&interval=1d&startTime={cursor}&limit=1000"
        with urllib.request.urlopen(url, timeout=30) as r:
            rows = json.load(r)
        if not rows:
            break
        for k in rows:
            out.append(
                {
                    "open_time": k[0],
                    "open": float(k[1]),
                    "high": float(k[2]),
                    "low": float(k[3]),
                    "close": float(k[4]),
                    "volume": float(k[5]),
                    "close_time": k[6],
                }
            )
        if len(rows) < 1000:
            break
        cursor = rows[-1][6] + 1
        time.sleep(0.15)
    return out


def main() -> int:
    args = [a for a in sys.argv[1:]]
    data_dir = "."
    start_ms = DEFAULT_START_MS
    if args and not args[0].startswith("--"):
        data_dir = args[0]
        args = args[1:]
    if "--start-ms" in args:
        start_ms = int(args[args.index("--start-ms") + 1])

    symbols = sorted(
        f[: -len("_1d.json")] for f in os.listdir(data_dir) if f.endswith("_1d.json")
    )
    if not symbols:
        print(f"目录 {data_dir} 下无 *_1d.json", file=sys.stderr)
        return 1

    backup = os.path.join(
        data_dir, "backup-" + datetime.now(timezone.utc).strftime("%Y%m%d-%H%M")
    )
    os.makedirs(backup, exist_ok=True)

    now_ms = int(time.time() * 1000)
    print(f"品种数 {len(symbols)} | 起点 {start_ms} | 备份目录 {backup}")
    failed = []
    for sym in symbols:
        path = os.path.join(data_dir, f"{sym}_1d.json")
        try:
            bars = fetch_symbol(sym, start_ms)
        except Exception as e:  # 网络/限频：记录后继续下一个品种
            print(f"  [FAIL] {sym}: {e}")
            failed.append(sym)
            continue
        bars = [b for b in bars if b["close_time"] <= now_ms]
        if len(bars) < 30:
            print(f"  [SKIP] {sym}: 仅 {len(bars)} 根，疑似异常，不覆盖")
            failed.append(sym)
            continue
        for i in range(1, len(bars)):
            if bars[i]["open_time"] <= bars[i - 1]["open_time"]:
                print(f"  [FAIL] {sym}: 时间戳非严格递增 @{i}")
                failed.append(sym)
                break
        else:
            os.replace(path, os.path.join(backup, f"{sym}_1d.json"))
            with open(path, "w") as fh:
                json.dump(bars, fh)
            d0 = datetime.fromtimestamp(bars[0]["open_time"] / 1000, timezone.utc)
            d1 = datetime.fromtimestamp(
                bars[-1]["open_time"] / 1000, timezone.utc
            )
            print(
                f"  [OK] {sym}: {len(bars)} 根 {d0:%Y-%m-%d} ~ {d1:%Y-%m-%d}"
            )
        time.sleep(0.15)
    if failed:
        print(f"失败/跳过: {failed}")
        return 1
    print("全部完成")
    return 0


if __name__ == "__main__":
    sys.exit(main())
