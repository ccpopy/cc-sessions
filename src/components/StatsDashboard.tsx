import { useEffect, useMemo, useState } from "react";
import { format } from "date-fns";
import type { EChartsCoreOption } from "echarts/core";
import {
  Activity,
  AlertCircle,
  Archive,
  ArrowUpRight,
  CalendarRange,
  Coins,
  FolderKanban,
  Gauge,
  MessageSquare,
  RefreshCw,
} from "lucide-react";
import { useNavigate } from "react-router-dom";

import { EChart } from "@/components/EChart";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";
import { FilterTabs } from "@/components/ui/filter-tabs";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  api,
  type Kpi,
  type ModelStat,
  type ProjectStat,
  type StatsProvider,
  type TimeseriesPoint,
} from "@/lib/api";
import { humanTokens } from "@/lib/format";
import { cn } from "@/lib/utils";
import { useSettings } from "@/stores/settings";
import { useTheme } from "@/stores/theme";
import { useView } from "@/stores/view";

type Range = "7d" | "30d" | "90d" | "all";
type Bucket = "day" | "week";
type ProjectMetric = "sessions" | "tokens";

const chartColors = [
  "hsl(var(--chart-1))",
  "hsl(var(--chart-2))",
  "hsl(var(--chart-3))",
  "hsl(var(--chart-4))",
  "hsl(var(--chart-5))",
];

const providerOptions: { value: StatsProvider; label: string }[] = [
  { value: "all", label: "全部" },
  { value: "codex", label: "Codex" },
  { value: "claude", label: "Claude" },
  { value: "opencode", label: "OpenCode" },
];

const rangeOptions: { value: Range; label: string }[] = [
  { value: "7d", label: "7 天" },
  { value: "30d", label: "30 天" },
  { value: "90d", label: "90 天" },
  { value: "all", label: "全部" },
];

const bucketOptions: { value: Bucket; label: string }[] = [
  { value: "day", label: "按日" },
  { value: "week", label: "按周" },
];

const preferredBucket: Record<Range, Bucket> = {
  "7d": "day",
  "30d": "day",
  "90d": "week",
  all: "week",
};

function rangeToTs(range: Range): [number | null, number | null] {
  const now = Math.floor(Date.now() / 1000);
  if (range === "all") return [null, null];
  const days = range === "7d" ? 7 : range === "30d" ? 30 : 90;
  return [now - days * 86400, now];
}

export function StatsDashboard() {
  const settings = useSettings((state) => state.settings);
  const [provider, setProvider] = useState<StatsProvider>("all");
  const [range, setRange] = useState<Range>("30d");
  const [bucket, setBucket] = useState<Bucket>("day");
  const [includeArchived, setIncludeArchived] = useState(false);
  const [tick, setTick] = useState(0);

  const [kpi, setKpi] = useState<Kpi | null>(null);
  const [timeseries, setTimeseries] = useState<TimeseriesPoint[]>([]);
  const [byProject, setByProject] = useState<ProjectStat[]>([]);
  const [byModel, setByModel] = useState<ModelStat[]>([]);
  const [heatmap, setHeatmap] = useState<number[][]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const changeRange = (next: Range) => {
    setRange(next);
    setBucket(preferredBucket[next]);
  };

  useEffect(() => {
    if (!settings) return;
    let cancelled = false;
    setKpi(null);
    setTimeseries([]);
    setByProject([]);
    setByModel([]);
    setHeatmap([]);
    setError(null);

    const requiredDir =
      provider === "claude"
        ? settings.claude_dir
        : provider === "opencode"
          ? settings.opencode_dir
          : settings.codex_dir;
    if (!requiredDir) {
      setLoading(false);
      setError(
        provider === "claude"
          ? "尚未配置 Claude 目录"
          : provider === "opencode"
            ? "尚未配置 OpenCode 数据目录"
            : "尚未配置 Codex 目录",
      );
      return;
    }

    const [from, to] = rangeToTs(range);
    const common = {
      provider,
      codex_dir: settings.codex_dir,
      claude_dir: settings.claude_dir,
      opencode_dir: settings.opencode_dir,
      from_ts: from,
      to_ts: to,
      cwd_filter: [] as string[],
      include_archived: includeArchived,
    };
    setLoading(true);
    void (async () => {
      try {
        const snapshot = await api.statsSnapshot({
          ...common,
          bucket,
          project_limit: 10,
        });
        if (cancelled) return;
        setKpi(snapshot.kpi);
        setTimeseries(snapshot.timeseries);
        setByProject(snapshot.by_project);
        setByModel(snapshot.by_model);
        setHeatmap(snapshot.heatmap);
      } catch (snapshotError) {
        if (!cancelled) setError(String((snapshotError as Error)?.message ?? snapshotError));
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [
    settings?.codex_dir,
    settings?.claude_dir,
    settings?.opencode_dir,
    provider,
    range,
    bucket,
    includeArchived,
    tick,
  ]);

  return (
    <div className="stats-dashboard relative mx-auto w-full max-w-[1540px] space-y-5 p-4 sm:p-6 lg:p-7">
      <section className="flex flex-wrap items-center justify-end gap-2">
        <FilterTabs
          label="Agent"
          value={provider}
          options={providerOptions}
          onChange={setProvider}
        />
        <FilterTabs label="统计范围" value={range} options={rangeOptions} onChange={changeRange} />
        <FilterTabs label="趋势粒度" value={bucket} options={bucketOptions} onChange={setBucket} />

        <div className="flex h-9 items-center gap-2 rounded-lg bg-card/75 px-2.5 ring-1 ring-inset ring-border/70">
          <Archive className="h-3.5 w-3.5 text-muted-foreground" />
          <Label htmlFor="stats-archived" className="cursor-pointer text-xs text-muted-foreground">
            含归档
          </Label>
          <Switch id="stats-archived" checked={includeArchived} onCheckedChange={setIncludeArchived} />
        </div>

        <Button
          variant="outline"
          size="icon"
          onClick={() => setTick((value) => value + 1)}
          className="h-9 w-9 bg-card/75 text-muted-foreground shadow-none hover:text-foreground"
          aria-label="刷新统计"
          title="刷新统计"
        >
          <RefreshCw className={cn("h-4 w-4", loading && "animate-spin")} />
        </Button>
      </section>

      {error && (
        <div
          role="alert"
          className="flex items-start gap-2 rounded-xl border border-destructive/35 bg-destructive/[0.07] p-3.5 text-sm text-destructive"
        >
          <AlertCircle className="mt-0.5 h-4 w-4 shrink-0" />
          <div className="min-w-0 whitespace-pre-wrap break-words">统计读取失败：{error}</div>
        </div>
      )}

      <KpiRow kpi={kpi} loading={loading} />

      <div className="grid grid-cols-1 gap-5 xl:grid-cols-12">
        <ActivityCard data={timeseries} bucket={bucket} loading={loading} />
        <ProjectRankingCard data={byProject} loading={loading} />
        <ModelDistributionCard data={byModel} loading={loading} />
        <HeatmapCard data={heatmap} loading={loading} />
      </div>
    </div>
  );
}

function KpiRow({ kpi, loading }: { kpi: Kpi | null; loading: boolean }) {
  const items = [
    {
      label: "会话总数",
      value: formatCount(kpi?.sessions_total ?? 0),
      icon: MessageSquare,
      color: chartColors[0],
    },
    {
      label: "Token 总量",
      value: humanTokens(kpi?.tokens_total ?? 0),
      icon: Coins,
      color: chartColors[1],
    },
    {
      label: "活跃项目",
      value: formatCount(kpi?.active_projects ?? 0),
      icon: FolderKanban,
      color: chartColors[3],
    },
    {
      label: "每会话用量",
      value: humanTokens(Math.round(kpi?.avg_tokens_per_session ?? 0)),
      icon: Gauge,
      color: chartColors[2],
    },
  ];

  return (
    <div className="grid grid-cols-2 gap-3 lg:grid-cols-4 lg:gap-4">
      {items.map((item) => (
        <Card
          key={item.label}
          className="group relative overflow-hidden border-border/70 bg-card/90 shadow-[0_1px_2px_hsl(var(--foreground)/0.04),0_16px_36px_-32px_hsl(var(--foreground)/0.45)] transition-[border-color,transform,box-shadow] duration-300 hover:-translate-y-0.5 hover:border-border hover:shadow-[0_10px_30px_-24px_hsl(var(--foreground)/0.38)]"
        >
          <span
            aria-hidden="true"
            className="absolute inset-x-0 top-0 h-[2px] opacity-75"
            style={{ background: `linear-gradient(90deg, ${item.color}, transparent 72%)` }}
          />
          <CardContent className="flex items-center gap-3 p-3.5 sm:gap-3.5 sm:p-4">
            <div
              className="flex h-10 w-10 shrink-0 items-center justify-center rounded-xl ring-1 ring-inset ring-border/50 transition-transform duration-300 group-hover:scale-105 sm:h-11 sm:w-11"
              style={{ backgroundColor: colorWithAlpha(item.color, 0.1), color: item.color }}
            >
              <item.icon className="h-[18px] w-[18px]" />
            </div>
            <div className="min-w-0">
              <div className="truncate text-[11px] font-semibold uppercase tracking-[0.12em] text-muted-foreground">
                {item.label}
              </div>
              <div className="mt-1 truncate text-xl font-semibold tabular-nums tracking-tight sm:text-2xl">
                {loading ? <Skeleton className="h-7 w-20" /> : item.value}
              </div>
            </div>
          </CardContent>
        </Card>
      ))}
    </div>
  );
}

function ActivityCard({
  data,
  bucket,
  loading,
}: {
  data: TimeseriesPoint[];
  bucket: Bucket;
  loading: boolean;
}) {
  const theme = useTheme((state) => state.resolved);
  const chartData = useMemo(
    () => data.map((point) => ({ ...point, label: formatBucket(point.bucket_start, bucket) })),
    [data, bucket],
  );
  const peak = useMemo(
    () => chartData.reduce<TimeseriesPoint & { label?: string } | null>((best, point) => (!best || point.sessions > best.sessions ? point : best), null),
    [chartData],
  );
  const chartOption = useMemo<EChartsCoreOption>(() => {
    const dark = theme === "dark";
    const axisColor = dark ? "rgba(212, 212, 216, 0.56)" : "rgba(82, 82, 91, 0.64)";
    const splitColor = dark ? "rgba(63, 63, 70, 0.54)" : "rgba(228, 228, 231, 0.8)";
    const tooltipBackground = dark ? "rgba(24, 24, 27, 0.96)" : "rgba(255, 255, 255, 0.97)";
    const tooltipBorder = dark ? "rgba(82, 82, 91, 0.9)" : "rgba(212, 212, 216, 0.95)";
    const textColor = dark ? "#f4f4f5" : "#18181b";
    const mutedText = dark ? "#a1a1aa" : "#71717a";
    const tokenColor = dark ? "#fb923c" : "#f97316";
    const barTop = dark ? "rgba(161, 161, 170, 0.46)" : "rgba(113, 113, 122, 0.34)";
    const barBottom = dark ? "rgba(82, 82, 91, 0.2)" : "rgba(161, 161, 170, 0.12)";

    return {
      animationDuration: 700,
      animationEasing: "cubicOut",
      grid: { top: 14, right: 50, bottom: 34, left: 38, containLabel: false },
      tooltip: {
        trigger: "axis",
        confine: true,
        backgroundColor: tooltipBackground,
        borderColor: tooltipBorder,
        borderWidth: 1,
        padding: 0,
        axisPointer: {
          type: "line",
          lineStyle: { color: dark ? "rgba(161,161,170,.42)" : "rgba(82,82,91,.28)", type: "dashed" },
        },
        formatter: (items: Array<{ axisValue?: string; seriesName?: string; value?: number; color?: string }>) => {
          const points = Array.isArray(items) ? items : [items];
          const sessions = points.find((item) => item.seriesName === "会话")?.value ?? 0;
          const tokens = points.find((item) => item.seriesName === "Token")?.value ?? 0;
          const label = escapeHtml(String(points[0]?.axisValue ?? ""));
          return `<div style="min-width:158px;padding:11px 12px;color:${textColor};font-size:12px"><div style="margin-bottom:8px;font-weight:600">${label}</div><div style="display:flex;justify-content:space-between;gap:24px;color:${mutedText}"><span>会话</span><b style="color:${textColor};font-weight:600">${formatCount(Number(sessions))} 条</b></div><div style="display:flex;justify-content:space-between;gap:24px;margin-top:6px;color:${mutedText}"><span>Token</span><b style="color:${textColor};font-weight:600">${humanTokens(Number(tokens))}</b></div></div>`;
        },
        extraCssText: "border-radius:12px;box-shadow:0 14px 36px rgba(0,0,0,.16);backdrop-filter:blur(10px);",
      },
      xAxis: {
        type: "category",
        boundaryGap: true,
        data: chartData.map((point) => point.label),
        axisLine: { show: false },
        axisTick: { show: false },
        axisLabel: { color: axisColor, fontSize: 10, margin: 12, hideOverlap: true },
      },
      yAxis: [
        {
          type: "value",
          minInterval: 1,
          axisLine: { show: false },
          axisTick: { show: false },
          axisLabel: { color: axisColor, fontSize: 10 },
          splitLine: { lineStyle: { color: splitColor, type: "dashed" } },
        },
        {
          type: "value",
          axisLine: { show: false },
          axisTick: { show: false },
          axisLabel: { color: axisColor, fontSize: 10, formatter: (value: number) => humanTokens(value) },
          splitLine: { show: false },
        },
      ],
      series: [
        {
          name: "会话",
          type: "bar",
          yAxisIndex: 0,
          data: chartData.map((point) => point.sessions),
          barMaxWidth: 16,
          itemStyle: {
            borderRadius: [4, 4, 1, 1],
            color: {
              type: "linear",
              x: 0,
              y: 0,
              x2: 0,
              y2: 1,
              colorStops: [
                { offset: 0, color: barTop },
                { offset: 1, color: barBottom },
              ],
            },
          },
          emphasis: { itemStyle: { opacity: 0.9 } },
        },
        {
          name: "Token",
          type: "line",
          yAxisIndex: 1,
          data: chartData.map((point) => point.tokens),
          smooth: 0.08,
          showSymbol: true,
          symbol: "circle",
          symbolSize: 6,
          lineStyle: { color: tokenColor, width: 2.5 },
          itemStyle: { color: tokenColor, borderColor: dark ? "#111114" : "#fff", borderWidth: 1.5 },
          areaStyle: {
            color: {
              type: "linear",
              x: 0,
              y: 0,
              x2: 0,
              y2: 1,
              colorStops: [
                { offset: 0, color: dark ? "rgba(251,146,60,.38)" : "rgba(249,115,22,.3)" },
                { offset: 0.78, color: dark ? "rgba(251,146,60,.08)" : "rgba(249,115,22,.05)" },
                { offset: 1, color: "rgba(249,115,22,0)" },
              ],
            },
          },
          emphasis: { focus: "series" },
        },
      ],
    };
  }, [chartData, theme]);

  return (
    <DashboardCard
      className="xl:col-span-7"
      title="活跃趋势"
      icon={Activity}
      accent={chartColors[0]}
      actions={
        <div className="flex items-center gap-4 text-[11px] font-medium text-muted-foreground">
          <Legend color="hsl(var(--muted-foreground) / 0.32)" label="会话" />
          <Legend color={chartColors[1]} label="Token" />
          {peak && <span className="hidden tabular-nums text-muted-foreground/80 sm:inline">峰值 {peak.sessions} 条</span>}
        </div>
      }
    >
      <div className="h-[320px] px-2 pb-3 pt-5 sm:px-4">
        {loading ? (
          <ChartSkeleton />
        ) : chartData.length === 0 ? (
          <EmptyChart />
        ) : (
          <EChart option={chartOption} />
        )}
      </div>
    </DashboardCard>
  );
}

function ProjectRankingCard({ data, loading }: { data: ProjectStat[]; loading: boolean }) {
  const [metric, setMetric] = useState<ProjectMetric>("tokens");
  const navigate = useNavigate();
  const setPrefill = useView((state) => state.setPrefillCwd);
  const setView = useView((state) => state.setView);
  const sorted = useMemo(() => [...data].sort((a, b) => b[metric] - a[metric]).slice(0, 8), [data, metric]);
  const maxValue = Math.max(1, sorted[0]?.[metric] ?? 0);

  const openProject = (project: ProjectStat) => {
    setPrefill(project.cwd);
    setView("project");
    navigate(`/${project.provider ?? "codex"}/sessions`);
  };

  return (
    <DashboardCard
      className="xl:col-span-5"
      title="项目排行"
      icon={FolderKanban}
      accent={chartColors[2]}
      actions={
        <Tabs value={metric} onValueChange={(value) => setMetric(value as ProjectMetric)}>
          <TabsList className="h-8 bg-muted/65 p-0.5">
            <TabsTrigger value="tokens" className="h-7 px-2.5 text-[11px] data-[state=active]:bg-card data-[state=active]:text-foreground">
              Token
            </TabsTrigger>
            <TabsTrigger value="sessions" className="h-7 px-2.5 text-[11px] data-[state=active]:bg-card data-[state=active]:text-foreground">
              会话
            </TabsTrigger>
          </TabsList>
        </Tabs>
      }
    >
      <div className="min-h-[320px] px-5 py-3">
        {loading ? (
          <RankingSkeleton />
        ) : sorted.length === 0 ? (
          <EmptyChart />
        ) : (
          <ol className="space-y-0.5">
            {sorted.map((project, index) => {
              const value = project[metric];
              const width = Math.max(3, (value / maxValue) * 100);
              return (
                <li key={`${project.provider ?? "unknown"}:${project.cwd}`}>
                  <button
                    type="button"
                    onClick={() => openProject(project)}
                    className="group grid w-full grid-cols-[22px_minmax(0,1fr)_auto] items-center gap-2 rounded-lg px-1.5 py-2 text-left transition-colors hover:bg-muted/55 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                    title={project.cwd}
                  >
                    <span className="text-[11px] font-medium tabular-nums text-muted-foreground/65">
                      {String(index + 1).padStart(2, "0")}
                    </span>
                    <span className="min-w-0">
                      <span className="flex min-w-0 items-center gap-1.5">
                        <ProviderDot provider={project.provider} />
                        <span className="min-w-0 truncate text-xs font-medium">{project.cwd_display}</span>
                      </span>
                      <span className="mt-1.5 block h-1.5 overflow-hidden rounded-full bg-muted">
                        <span
                          className="block h-full rounded-full transition-[width] duration-500"
                          style={{
                            width: `${width}%`,
                            background: `linear-gradient(90deg, ${chartColors[0]}, ${chartColors[2]})`,
                          }}
                        />
                      </span>
                    </span>
                    <span className="flex min-w-[76px] items-center justify-end gap-1 text-xs tabular-nums">
                      <span className="font-semibold">
                        {metric === "tokens" ? humanTokens(value) : formatCount(value)}
                      </span>
                      <span className="text-[10px] text-muted-foreground">{metric === "tokens" ? "tok" : "条"}</span>
                      <ArrowUpRight className="h-3 w-3 text-muted-foreground/0 transition-colors group-hover:text-muted-foreground" />
                    </span>
                  </button>
                </li>
              );
            })}
          </ol>
        )}
      </div>
    </DashboardCard>
  );
}

/** 模型清单最多直接展示的行数，超出部分聚合为“其他”，保证卡片不出现内部滚动。 */
const MODEL_ROW_LIMIT = 5;
const otherSliceColor = "hsl(var(--muted-foreground) / 0.4)";

type ModelRow = {
  key: string;
  name: string;
  sessions: number;
  tokens: number;
  color: string;
};

function ModelDistributionCard({ data, loading }: { data: ModelStat[]; loading: boolean }) {
  const theme = useTheme((state) => state.resolved);
  const totalSessions = useMemo(() => data.reduce((sum, model) => sum + model.sessions, 0), [data]);
  const rows = useMemo<ModelRow[]>(() => {
    const aggregated = new Map<string, { sessions: number; tokens: number }>();
    for (const model of data) {
      const name = model.model || "(未标注)";
      const current = aggregated.get(name) ?? { sessions: 0, tokens: 0 };
      current.sessions += model.sessions;
      current.tokens += model.tokens;
      aggregated.set(name, current);
    }
    const sorted = [...aggregated.entries()]
      .map(([name, totals]) => ({ name, ...totals }))
      .sort((a, b) => b.sessions - a.sessions || b.tokens - a.tokens);
    const top = sorted.length <= MODEL_ROW_LIMIT + 1 ? sorted : sorted.slice(0, MODEL_ROW_LIMIT);
    const rest = sorted.slice(top.length);
    const result = top.map((model, index) => ({
      key: model.name,
      name: model.name,
      sessions: model.sessions,
      tokens: model.tokens,
      color: index < chartColors.length ? chartColors[index] : otherSliceColor,
    }));
    if (rest.length > 0) {
      result.push({
        key: "__others",
        name: "其他",
        sessions: rest.reduce((sum, model) => sum + model.sessions, 0),
        tokens: rest.reduce((sum, model) => sum + model.tokens, 0),
        color: otherSliceColor,
      });
    }
    return result;
  }, [data]);
  const chartOption = useMemo<EChartsCoreOption>(() => {
    const dark = theme === "dark";
    const textColor = dark ? "#f4f4f5" : "#18181b";
    const mutedText = dark ? "#a1a1aa" : "#71717a";
    const background = dark ? "rgba(24,24,27,.96)" : "rgba(255,255,255,.97)";
    const border = dark ? "rgba(82,82,91,.9)" : "rgba(212,212,216,.95)";
    const palette = dark
      ? ["#60a5fa", "#fb923c", "#a78bfa", "#2dd4bf", "#f472b6"]
      : ["#3b82f6", "#f97316", "#8b5cf6", "#14b8a6", "#e11d48"];
    const chartRows = rows.map((row, index) => ({
      ...row,
      value: row.sessions,
      chartColor: row.key === "__others" ? (dark ? "#71717a" : "#a1a1aa") : palette[index % palette.length],
    }));
    return {
      animationDuration: 750,
      animationEasing: "cubicOut",
      tooltip: {
        trigger: "item",
        confine: true,
        backgroundColor: background,
        borderColor: border,
        borderWidth: 1,
        padding: 0,
        formatter: (item: { data?: ModelRow & { value: number; chartColor: string }; percent?: number }) => {
          const row = item.data;
          if (!row) return "";
          return `<div style="min-width:176px;max-width:250px;padding:10px 12px;color:${textColor};font-size:12px"><div style="display:flex;align-items:center;gap:7px"><i style="width:8px;height:8px;border-radius:999px;background:${row.chartColor};flex:none"></i><b style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font-weight:600">${escapeHtml(row.name)}</b></div><div style="display:flex;justify-content:space-between;gap:18px;margin-top:8px;color:${mutedText}"><span><b style="color:${textColor}">${row.value}</b> 条 · ${item.percent ?? 0}%</span><span>${humanTokens(row.tokens)} token</span></div></div>`;
        },
        extraCssText: "border-radius:10px;box-shadow:0 14px 36px rgba(0,0,0,.16);backdrop-filter:blur(10px);",
      },
      series: [
        {
          type: "pie",
          radius: ["68%", "88%"],
          center: ["50%", "50%"],
          avoidLabelOverlap: true,
          padAngle: rows.length > 1 ? 2 : 0,
          itemStyle: { borderRadius: 5, borderColor: dark ? "#151518" : "#fff", borderWidth: 2 },
          label: { show: false },
          labelLine: { show: false },
          emphasis: { scale: true, scaleSize: 5, focus: "self" },
          data: chartRows.map((row) => ({ ...row, itemStyle: { color: row.chartColor } })),
        },
      ],
    };
  }, [rows, theme]);

  return (
    <DashboardCard className="xl:col-span-7" title="模型分布" icon={Gauge} accent={chartColors[3]}>
      <div className="grid min-h-[330px] grid-cols-1 items-center gap-3 px-5 py-4 sm:grid-cols-[210px_minmax(0,1fr)] sm:gap-6">
        {loading ? (
          <>
            <Skeleton className="mx-auto h-40 w-40 rounded-full" />
            <RankingSkeleton rows={5} />
          </>
        ) : rows.length === 0 ? (
          <div className="sm:col-span-2">
            <EmptyChart />
          </div>
        ) : (
          <>
            <div className="relative mx-auto h-[190px] w-[190px]">
              <EChart option={chartOption} />
              <div className="pointer-events-none absolute inset-0 flex flex-col items-center justify-center">
                <span className="text-2xl font-semibold tabular-nums tracking-tight">{formatCount(totalSessions)}</span>
                <span className="mt-0.5 text-[10px] uppercase tracking-[0.15em] text-muted-foreground">会话</span>
              </div>
            </div>

            <ul className="min-w-0 divide-y divide-border/50">
              {rows.map((row) => (
                <li key={row.key} className="flex min-w-0 items-center gap-2.5 py-3 first:pt-0 last:pb-0">
                  <span
                    className="h-2 w-2 shrink-0 rounded-full shadow-[0_0_0_3px_hsl(var(--background))]"
                    style={{ backgroundColor: row.color }}
                  />
                  <span className="min-w-0 flex-1 truncate text-xs font-medium" title={row.name}>
                    {row.name}
                  </span>
                  <span className="shrink-0 text-xs tabular-nums text-muted-foreground">
                    <b className="font-semibold text-foreground">{row.sessions} 条</b>
                    <span className="mx-1.5" />
                    {humanTokens(row.tokens)} token
                  </span>
                </li>
              ))}
            </ul>
          </>
        )}
      </div>
    </DashboardCard>
  );
}

function HeatmapCard({ data, loading }: { data: number[][]; loading: boolean }) {
  const dayNames = ["日", "一", "二", "三", "四", "五", "六"];
  const max = useMemo(() => Math.max(1, ...data.flat()), [data]);
  const total = useMemo(() => data.flat().reduce((sum, value) => sum + value, 0), [data]);
  const peak = useMemo(() => {
    let result = { day: 0, hour: 0, value: 0 };
    data.forEach((row, day) => {
      row.forEach((value, hour) => {
        if (value > result.value) result = { day, hour, value };
      });
    });
    return result;
  }, [data]);
  const levels = [0, 0.2, 0.42, 0.68, 1];
  const levelFor = (value: number) => {
    if (value <= 0) return 0;
    const ratio = value / max;
    if (ratio < 0.25) return 1;
    if (ratio < 0.5) return 2;
    if (ratio < 0.75) return 3;
    return 4;
  };

  return (
    <DashboardCard
      className="xl:col-span-5"
      title="活跃时段"
      icon={CalendarRange}
      accent={chartColors[4]}
      actions={
        !loading && peak.value > 0 ? (
          <span className="whitespace-nowrap text-[11px] tabular-nums text-muted-foreground">
            峰值 周{dayNames[peak.day]} {String(peak.hour).padStart(2, "0")}:00
          </span>
        ) : undefined
      }
    >
      <div className="flex min-h-[330px] flex-col justify-center px-5 py-4">
        {loading ? (
          <ChartSkeleton />
        ) : data.length === 0 ? (
          <EmptyChart />
        ) : (
          <>
            <div className="mb-5 flex items-end justify-between gap-4">
              <div>
                <div className="text-2xl font-semibold tabular-nums tracking-tight">{formatCount(total)}</div>
                <div className="mt-0.5 text-[11px] text-muted-foreground">累计活跃次数</div>
              </div>
              <div className="flex items-center gap-1.5 text-[10px] text-muted-foreground">
                <span>少</span>
                {levels.map((opacity, index) => (
                  <span
                    key={index}
                    className={cn("h-2.5 w-2.5 rounded-[3px]", index === 0 && "border border-border/70 bg-muted/70")}
                    style={index === 0 ? undefined : { backgroundColor: colorWithAlpha(chartColors[0], opacity) }}
                  />
                ))}
                <span>多</span>
              </div>
            </div>

            <div>
              <div className="mb-1.5 flex items-center gap-[3px]">
                <div className="w-6 shrink-0" />
                <div className="grid flex-1 grid-cols-[repeat(24,minmax(0,1fr))] gap-[3px]">
                  {Array.from({ length: 24 }).map((_, hour) => (
                    <div key={hour} className="text-center text-[9px] leading-none text-muted-foreground/70">
                      {hour % 3 === 0 ? hour : ""}
                    </div>
                  ))}
                </div>
              </div>
              <div className="flex flex-col gap-[3px]">
                {data.map((row, day) => (
                  <div key={day} className="flex items-center gap-[3px]">
                    <div className="w-6 shrink-0 pr-1 text-right text-[10px] text-muted-foreground">{dayNames[day]}</div>
                    <div className="grid flex-1 grid-cols-[repeat(24,minmax(0,1fr))] gap-[3px]">
                      {row.map((value, hour) => {
                        const level = levelFor(value);
                        return (
                          <div
                            key={hour}
                            title={`周${dayNames[day]} ${String(hour).padStart(2, "0")}:00 · ${value} 次`}
                            className={cn(
                              "aspect-square w-full rounded-[3px] ring-1 ring-inset ring-transparent transition-[transform,filter] hover:z-10 hover:scale-125 hover:brightness-110",
                              level === 0 && "bg-muted/70 ring-border/50",
                            )}
                            style={level === 0 ? undefined : { backgroundColor: colorWithAlpha(chartColors[0], levels[level]) }}
                          />
                        );
                      })}
                    </div>
                  </div>
                ))}
              </div>
            </div>
          </>
        )}
      </div>
    </DashboardCard>
  );
}

function DashboardCard({
  title,
  icon: Icon,
  accent,
  actions,
  className,
  children,
}: {
  title: string;
  icon: typeof Activity;
  accent: string;
  actions?: React.ReactNode;
  className?: string;
  children: React.ReactNode;
}) {
  return (
    <Card
      className={cn(
        "overflow-hidden border-border/70 bg-card/90 shadow-[0_1px_2px_hsl(var(--foreground)/0.04),0_18px_44px_-38px_hsl(var(--foreground)/0.42)]",
        className,
      )}
    >
      <CardHeader className="flex-row flex-wrap items-center justify-between gap-x-4 gap-y-2 space-y-0 border-b border-border/60 px-5 py-3.5">
        <CardTitle className="flex items-center gap-2.5 text-sm">
          <span
            className="flex h-7 w-7 items-center justify-center rounded-lg ring-1 ring-inset ring-border/40"
            style={{ backgroundColor: colorWithAlpha(accent, 0.09), color: accent }}
          >
            <Icon className="h-3.5 w-3.5" />
          </span>
          <span>{title}</span>
        </CardTitle>
        {actions}
      </CardHeader>
      <CardContent className="p-0">{children}</CardContent>
    </Card>
  );
}

function ProviderDot({ provider }: { provider: ProjectStat["provider"] }) {
  const color =
    provider === "claude"
      ? "hsl(var(--provider-claude))"
      : provider === "opencode"
        ? "hsl(var(--provider-opencode))"
        : "hsl(var(--provider-codex))";
  return <span className="h-1.5 w-1.5 shrink-0 rounded-full" style={{ backgroundColor: color }} />;
}

function Legend({ color, label }: { color: string; label: string }) {
  return (
    <span className="flex items-center gap-1.5">
      <span className="h-1.5 w-3 rounded-full" style={{ backgroundColor: color }} />
      {label}
    </span>
  );
}

function ChartSkeleton() {
  return (
    <div className="flex h-full items-end gap-2 px-2 pb-6">
      {[38, 68, 46, 82, 58, 74, 34, 62, 88, 52, 70, 42].map((height, index) => (
        <Skeleton key={index} className="min-w-2 flex-1 rounded-t-md" style={{ height: `${height}%` }} />
      ))}
    </div>
  );
}

function RankingSkeleton({ rows = 7 }: { rows?: number }) {
  return (
    <div className="space-y-3 py-1">
      {Array.from({ length: rows }).map((_, index) => (
        <div key={index} className="flex items-center gap-3">
          <Skeleton className="h-2 w-2 rounded-full" />
          <div className="flex-1 space-y-1.5">
            <Skeleton className="h-3" style={{ width: `${72 - index * 4}%` }} />
            <Skeleton className="h-1.5 w-full rounded-full" />
          </div>
          <Skeleton className="h-3 w-12" />
        </div>
      ))}
    </div>
  );
}

function EmptyChart() {
  return (
    <div className="flex h-full min-h-40 flex-col items-center justify-center text-center">
      <div className="flex h-10 w-10 items-center justify-center rounded-full bg-muted text-muted-foreground">
        <Activity className="h-4 w-4" />
      </div>
      <div className="mt-3 text-xs font-medium text-muted-foreground">当前筛选范围暂无数据</div>
    </div>
  );
}

function formatBucket(timestamp: number, bucket: Bucket): string {
  const date = new Date(timestamp * 1000);
  if (bucket === "week") return `W${getIsoWeek(date)} · ${format(date, "MM-dd")}`;
  return format(date, "MM-dd");
}

function getIsoWeek(date: Date): number {
  const target = new Date(date.valueOf());
  const dayNumber = (date.getDay() + 6) % 7;
  target.setDate(target.getDate() - dayNumber + 3);
  const firstThursday = new Date(target.getFullYear(), 0, 4);
  const difference = (target.getTime() - firstThursday.getTime()) / 86400000;
  return 1 + Math.round(difference / 7);
}

function formatCount(value: number): string {
  return new Intl.NumberFormat("zh-CN").format(value);
}

function colorWithAlpha(color: string, alpha: number): string {
  return color.replace(/\)\s*$/, ` / ${alpha})`);
}

function escapeHtml(value: string): string {
  return value.replace(/[&<>'"]/g, (character) => {
    const entities: Record<string, string> = {
      "&": "&amp;",
      "<": "&lt;",
      ">": "&gt;",
      "'": "&#39;",
      '"': "&quot;",
    };
    return entities[character];
  });
}
