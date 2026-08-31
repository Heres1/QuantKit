// 图表组件：lightweight-charts（TradingView 开源），蜡烛图 + 权益曲线（支持基准叠加与多曲线对比）


import { useEffect, useRef } from "react";
import {
  ColorType,
  createChart,
  IChartApi,
  ISeriesApi,
  UTCTimestamp,
} from "lightweight-charts";
import type { EquityPoint, Kline } from "./types";

const CHART_OPTS = {
  layout: {
    background: { type: ColorType.Solid, color: "#0e1117" },
    textColor: "#9aa4b2",
  },
  grid: {
    vertLines: { color: "#1c2230" },
    horzLines: { color: "#1c2230" },
  },
  autoSize: true,
  timeScale: { timeVisible: true, borderColor: "#2a3242" },
};

// lightweight-charts 一律按 UTC 解释和渲染时间戳；这里把毫秒时间戳换算成
// “本地墙钟时间对应的伪 UTC”，使坐标轴刻度与十字光标提示显示浏览器本地时间
// （按每个点各自取偏移，天然兼容夏令时地区）
function toLocalTime(ms: number): UTCTimestamp {
  return (Math.floor(ms / 1000) - new Date(ms).getTimezoneOffset() * 60) as UTCTimestamp;
}

export function CandleChart({ klines, height = 420 }: { klines: Kline[]; height?: number }) {
  const ref = useRef<HTMLDivElement>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const seriesRef = useRef<ISeriesApi<"Candlestick"> | null>(null);

  useEffect(() => {
    if (!ref.current) return;
    const chart = createChart(ref.current, CHART_OPTS);
    const series = chart.addCandlestickSeries({
      upColor: "#26a69a",
      downColor: "#ef5350",
      borderVisible: false,
      wickUpColor: "#26a69a",
      wickDownColor: "#ef5350",
    });
    chartRef.current = chart;
    seriesRef.current = series;
    return () => {
      chart.remove();
      chartRef.current = null;
      seriesRef.current = null;
    };
  }, []);

  useEffect(() => {
    const s = seriesRef.current;
    if (!s) return;
    s.setData(
      klines.map((k) => ({
        time: toLocalTime(k.open_time),
        open: k.open,
        high: k.high,
        low: k.low,
        close: k.close,
      }))
    );
    chartRef.current?.timeScale().fitContent();
  }, [klines]);

  return <div ref={ref} style={{ width: "100%", height }} />;
}

// 曲线去重：同一毫秒多个点会导致时间轴冲突，保留最后一个
type LinePoint = { time: UTCTimestamp; value: number };

function dedupPoints(points: { timestamp: number; value: number }[]): LinePoint[] {
  const byTime = new Map<number, number>();
  for (const p of points) byTime.set(p.timestamp, p.value);
  return [...byTime.entries()].map(([t, v]) => ({
    time: toLocalTime(t),
    value: v,
  }));
}

export function EquityChart({
  curve,
  benchmark,
  height = 320,
}: {
  curve: EquityPoint[];
  /** 基准权益曲线（同初始资金），叠加显示为虚线 */
  benchmark?: EquityPoint[];
  height?: number;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const seriesRef = useRef<ISeriesApi<"Line"> | null>(null);
  const benchRef = useRef<ISeriesApi<"Line"> | null>(null);

  useEffect(() => {
    if (!ref.current) return;
    const chart = createChart(ref.current, CHART_OPTS);
    const series = chart.addLineSeries({
      color: "#5b8def",
      lineWidth: 2,
      crosshairMarkerVisible: false,
    });
    const bench = chart.addLineSeries({
      color: "#f5a623",
      lineWidth: 2,
      lineStyle: 2, // dashed
      crosshairMarkerVisible: false,
    });
    chartRef.current = chart;
    seriesRef.current = series;
    benchRef.current = bench;
    return () => {
      chart.remove();
      chartRef.current = null;
      seriesRef.current = null;
      benchRef.current = null;
    };
  }, []);

  useEffect(() => {
    const s = seriesRef.current;
    if (!s) return;
    s.setData(dedupPoints(curve.map((p) => ({ timestamp: p.timestamp, value: p.equity }))));
    benchRef.current?.setData(
      benchmark
        ? dedupPoints(benchmark.map((p) => ({ timestamp: p.timestamp, value: p.equity })))
        : [],
    );
    chartRef.current?.timeScale().fitContent();
  }, [curve, benchmark]);

  return <div ref={ref} style={{ width: "100%", height }} />;
}

// 多曲线对比图：归一化权益曲线叠加（每条曲线自行归一到 100）
export function EquityChartMulti({
  series,
  height = 360,
}: {
  series: { name: string; color: string; points: EquityPoint[] }[];
  height?: number;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const linesRef = useRef<ISeriesApi<"Line">[]>([]);

  useEffect(() => {
    if (!ref.current) return;
    const chart = createChart(ref.current, CHART_OPTS);
    chartRef.current = chart;
    linesRef.current = [];
    return () => {
      chart.remove();
      chartRef.current = null;
      linesRef.current = [];
    };
  }, []);

  useEffect(() => {
    const chart = chartRef.current;
    if (!chart) return;
    // 重建系列（数量可变）：先清旧再按新数据加线，归一到首点=100
    for (const s of linesRef.current) chart.removeSeries(s);
    linesRef.current = series.map((sr) => {
      const line = chart.addLineSeries({
        color: sr.color,
        lineWidth: 2,
        title: sr.name,
        crosshairMarkerVisible: false,
      });
      const base = sr.points.length > 0 ? sr.points[0].equity : 1;
      line.setData(
        dedupPoints(
          sr.points.map((p) => ({
            timestamp: p.timestamp,
            value: base > 0 ? (p.equity / base) * 100 : 0,
          })),
        ),
      );
      return line;
    });
    chart.timeScale().fitContent();
  }, [series]);

  return <div ref={ref} style={{ width: "100%", height }} />;
}

// 回撤面积图：权益曲线相对历史峰值的回撤（%，≤0），红色系警示配色
export function DrawdownChart({
  points,
  height = 200,
}: {
  points: { ts: number; dd_pct: number }[];
  height?: number;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const seriesRef = useRef<ISeriesApi<"Area"> | null>(null);

  useEffect(() => {
    if (!ref.current) return;
    const chart = createChart(ref.current, CHART_OPTS);
    const series = chart.addAreaSeries({
      lineColor: "#ef5350",
      topColor: "rgba(239, 83, 80, 0.35)",
      bottomColor: "rgba(239, 83, 80, 0.02)",
      lineWidth: 2,
      crosshairMarkerVisible: false,
    });
    chartRef.current = chart;
    seriesRef.current = series;
    return () => {
      chart.remove();
      chartRef.current = null;
      seriesRef.current = null;
    };
  }, []);

  useEffect(() => {
    const s = seriesRef.current;
    if (!s) return;
    s.setData(dedupPoints(points.map((p) => ({ timestamp: p.ts, value: p.dd_pct }))));
    chartRef.current?.timeScale().fitContent();
  }, [points]);

  return <div ref={ref} style={{ width: "100%", height }} />;
}
