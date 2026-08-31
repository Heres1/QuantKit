// 本地持久化：回测品种选择 + 最近一次回测参数（跨页面共享）

import type { RunParams } from "./api";

const KEY = "quantkit_bt_symbols";
const BT_PARAMS_KEY = "quantkit_last_bt_params";

export function getBtSymbols(): string[] {
  try {
    const v = JSON.parse(localStorage.getItem(KEY) || "[]");
    return Array.isArray(v) ? v.filter((s) => typeof s === "string") : [];
  } catch {
    return [];
  }
}

export function setBtSymbols(list: string[]) {
  localStorage.setItem(KEY, JSON.stringify(list));
}

export function addBtSymbol(sym: string) {
  const list = getBtSymbols();
  if (!list.includes(sym)) {
    list.push(sym);
    setBtSymbols(list);
  }
}

export function removeBtSymbol(sym: string) {
  setBtSymbols(getBtSymbols().filter((s) => s !== sym));
}

/** 最近一次回测用的运行相关参数：供模拟盘/实盘页「沿用回测中心参数」读取 */
export function getLastBtParams(): RunParams | null {
  try {
    const v = JSON.parse(localStorage.getItem(BT_PARAMS_KEY) || "null");
    return v && typeof v === "object" && !Array.isArray(v) ? (v as RunParams) : null;
  } catch {
    return null;
  }
}

export function setLastBtParams(p: RunParams) {
  localStorage.setItem(BT_PARAMS_KEY, JSON.stringify(p));
}
