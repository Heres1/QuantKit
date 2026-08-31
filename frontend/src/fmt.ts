// 展示格式化

export function fmtPrice(p: number): string {
  if (!isFinite(p)) return "-";
  if (p >= 1000) return p.toLocaleString("en-US", { maximumFractionDigits: 2 });
  if (p >= 1) return p.toFixed(4);
  return p.toPrecision(4);
}

export function fmtPct(v: number): string {
  return `${v >= 0 ? "+" : ""}${v.toFixed(2)}%`;
}

export function fmtNum(v: number, digits = 2): string {
  if (!isFinite(v)) return "-";
  return v.toLocaleString("en-US", { maximumFractionDigits: digits });
}

export function fmtVol(v: number): string {
  if (!isFinite(v)) return "-";
  // 负值（如净资金流为净流出）按绝对值压缩并保留符号
  const a = Math.abs(v);
  const s =
    a >= 1e9
      ? (a / 1e9).toFixed(2) + "B"
      : a >= 1e6
        ? (a / 1e6).toFixed(2) + "M"
        : a >= 1e3
          ? (a / 1e3).toFixed(1) + "K"
          : a.toFixed(0);
  return v < 0 ? `-${s}` : s;
}

export function fmtDate(ms: number): string {
  if (!ms) return "-";
  return new Date(ms).toISOString().slice(0, 10);
}

export function fmtDateTime(ms: number): string {
  if (!ms) return "-";
  return new Date(ms).toISOString().slice(0, 19).replace("T", " ");
}
