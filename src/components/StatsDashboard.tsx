import { useEffect, useMemo, useState } from "react";
import { format } from "date-fns";
import {
  Area,
  AreaChart,
  Bar,
  BarChart,
  CartesianGrid,
  Cell,
  Line,
  LineChart,
  Pie,
  PieChart,
  ResponsiveContainer,
  Tooltip as RTooltip,
  XAxis,
  YAxis,
} from "recharts";
import { CalendarDays, CalendarRange, Coins, FolderKanban, Gauge, MessageSquare, RefreshCw } from "lucide-react";

import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Switch } from "@/components/ui/switch";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { useSettings } from "@/stores/settings";
import {
  api,
  type Kpi,
  type ModelStat,
  type ProjectStat,
  type StatsProvider,
  type TimeseriesPoint,
} from "@/lib/api";
import { humanTokens, relativeTime } from "@/lib/format";
import { cn } from "@/lib/utils";
import { useNavigate } from "react-router-dom";
import { useView } from "@/stores/view";

type Range = "7d" | "30d" | "90d" | "all";
type Bucket = "day" | "week";

const chartColors = [
  "hsl(var(--chart-1))",
  "hsl(var(--chart-2))",
  "hsl(var(--chart-3))",
  "hsl(var(--chart-4))",
  "hsl(var(--chart-5))",
];

function rangeToTs(r: Range): [number | null, number | null] {
  const now = Math.floor(Date.now() / 1000);
  if (r === "all") return [null, null];
  const days = r === "7d" ? 7 : r === "30d" ? 30 : 90;
  return [now - days * 86400, now];
}

export function StatsDashboard() {
  const settings = useSettings((s) => s.settings);
  const [provider, setProvider] = useState<StatsProvider>("all");
  const [range, setRange] = useState<Range>("30d");
  const [bucket, setBucket] = useState<Bucket>("day");
  const [includeArchived, setIncludeArchived] = useState(false);
  const [tick, setTick] = useState(0);

  const [kpi, setKpi] = useState<Kpi | null>(null);
  const [ts, setTs] = useState<TimeseriesPoint[]>([]);
  const [byProject, setByProject] = useState<ProjectStat[]>([]);
  const [byModel, setByModel] = useState<ModelStat[]>([]);
  const [heat, setHeat] = useState<number[][]>([]);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    if (!settings?.codex_dir) return;
    const [from, to] = rangeToTs(range);
    const providerDir = provider === "gemini" ? settings.gemini_dir : settings.claude_dir;
    const common = {
      provider,
      codex_dir: settings.codex_dir,
      claude_dir: providerDir,
      from_ts: from,
      to_ts: to,
      cwd_filter: [] as string[],
      include_archived: includeArchived,
    };
    setLoading(true);
    Promise.all([
      api.statsKpi(common),
      api.statsTimeseries({ ...common, bucket }),
      api.statsByProject({ ...common, limit: 10 }),
      api.statsByModel(common),
      api.statsHeatmap(common),
    ])
      .then(([k, t, p, m, h]) => {
        setKpi(k);
        setTs(t);
        setByProject(p);
        setByModel(m);
        setHeat(h);
      })
      .finally(() => setLoading(false));
  }, [settings?.codex_dir, settings?.claude_dir, settings?.gemini_dir, provider, range, bucket, includeArchived, tick]);

  return (
    <div className="space-y-4 p-6">
      <div className="flex flex-wrap items-center gap-3 rounded-lg border bg-card p-3 shadow-sm">
        <Tabs value={provider} onValueChange={(v) => setProvider(v as StatsProvider)}>
          <TabsList className="h-9">
            <TabsTrigger value="all">全部</TabsTrigger>
            <TabsTrigger value="codex">Codex</TabsTrigger>
            <TabsTrigger value="claude">Claude</TabsTrigger>
            <TabsTrigger value="gemini">Gemini</TabsTrigger>
          </TabsList>
        </Tabs>
        <Select value={range} onValueChange={(v) => setRange(v as Range)}>
          <SelectTrigger className="w-36">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="7d">过去 7 天</SelectItem>
            <SelectItem value="30d">过去 30 天</SelectItem>
            <SelectItem value="90d">过去 90 天</SelectItem>
            <SelectItem value="all">全部</SelectItem>
          </SelectContent>
        </Select>
        <Tabs value={bucket} onValueChange={(v) => setBucket(v as Bucket)}>
          <TabsList className="h-9">
            <TabsTrigger value="day" className="gap-1.5">
              <CalendarDays className="h-3.5 w-3.5" />
              按日
            </TabsTrigger>
            <TabsTrigger value="week" className="gap-1.5">
              <CalendarRange className="h-3.5 w-3.5" />
              按周
            </TabsTrigger>
          </TabsList>
        </Tabs>
        <div className="flex items-center gap-2">
          <Switch id="arc" checked={includeArchived} onCheckedChange={setIncludeArchived} />
          <Label htmlFor="arc" className="text-xs">
            包含已归档
          </Label>
        </div>
        <Button variant="ghost" size="sm" onClick={() => setTick((x) => x + 1)} className="ml-auto gap-1">
          <RefreshCw className={"h-4 w-4 " + (loading ? "animate-spin" : "")} />
          刷新
        </Button>
      </div>

      <KpiRow k={kpi} loading={loading} />

      <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader className="pb-2">
            <CardTitle className="text-sm">频次趋势</CardTitle>
          </CardHeader>
          <CardContent className="h-64">
            <ResponsiveContainer width="100%" height="100%">
              <LineChart data={ts.map((t) => ({ ...t, label: formatBucket(t.bucket_start, bucket) }))}>
                <CartesianGrid strokeDasharray="3 3" className="stroke-muted" />
                <XAxis dataKey="label" fontSize={11} />
                <YAxis fontSize={11} allowDecimals={false} />
                <RTooltip />
                <Line type="monotone" dataKey="sessions" stroke={chartColors[0]} strokeWidth={2} dot={false} />
              </LineChart>
            </ResponsiveContainer>
          </CardContent>
        </Card>

        <Card>
          <CardHeader className="pb-2">
            <CardTitle className="text-sm">Token 趋势</CardTitle>
          </CardHeader>
          <CardContent className="h-64">
            <ResponsiveContainer width="100%" height="100%">
              <AreaChart data={ts.map((t) => ({ ...t, label: formatBucket(t.bucket_start, bucket) }))}>
                <CartesianGrid strokeDasharray="3 3" className="stroke-muted" />
                <XAxis dataKey="label" fontSize={11} />
                <YAxis fontSize={11} tickFormatter={(v) => humanTokens(Number(v))} />
                <RTooltip formatter={(v: number) => humanTokens(v)} />
                <Area type="monotone" dataKey="tokens" stroke={chartColors[1]} fill={chartColors[1]} fillOpacity={0.25} />
              </AreaChart>
            </ResponsiveContainer>
          </CardContent>
        </Card>

        <ProjectTopCard title="项目 Top 10 — 会话数" data={byProject} kind="sessions" />
        <ProjectTopCard title="项目 Top 10 — Token" data={byProject} kind="tokens" />

        <ModelDistCard data={byModel} />

        <HeatmapCard data={heat} />
      </div>
    </div>
  );
}

function KpiRow({ k, loading }: { k: Kpi | null; loading: boolean }) {
  const items = [
    { label: "会话总数", value: k?.sessions_total ?? 0, icon: MessageSquare },
    { label: "Token 总量", value: humanTokens(k?.tokens_total ?? 0), icon: Coins },
    { label: "活跃项目", value: k?.active_projects ?? 0, icon: FolderKanban },
    {
      label: "平均每会话 token",
      value: humanTokens(Math.round(k?.avg_tokens_per_session ?? 0)),
      icon: Gauge,
    },
  ];
  return (
    <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
      {items.map((it) => (
        <Card key={it.label}>
          <CardContent className="flex items-center gap-3 p-4">
            <div className="flex h-9 w-9 items-center justify-center rounded-md bg-muted">
              <it.icon className="h-4 w-4 text-muted-foreground" />
            </div>
            <div className="min-w-0">
              <div className="text-xs text-muted-foreground">{it.label}</div>
              <div className="text-xl font-semibold">
                {loading ? <Skeleton className="h-6 w-16" /> : it.value}
              </div>
            </div>
          </CardContent>
        </Card>
      ))}
    </div>
  );
}

function ProjectTopCard({
  title,
  data,
  kind,
}: {
  title: string;
  data: ProjectStat[];
  kind: "sessions" | "tokens";
}) {
  const nav = useNavigate();
  const setPrefill = useView((s) => s.setPrefillCwd);
  const setView = useView((s) => s.setView);
  const sorted = useMemo(
    () =>
      [...data]
        .sort((a, b) => b[kind] - a[kind])
        .slice(0, 10)
        .map((x) => ({
          ...x,
          label: x.provider ? `[${providerLabel(x.provider)}] ${x.cwd_display}` : x.cwd_display,
        })),
    [data, kind],
  );
  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-sm">{title}</CardTitle>
      </CardHeader>
      <CardContent className="h-64">
        <ResponsiveContainer width="100%" height="100%">
          <BarChart data={sorted} layout="vertical" margin={{ left: 12, right: 12 }}>
            <CartesianGrid strokeDasharray="3 3" className="stroke-muted" horizontal={false} />
            <XAxis
              type="number"
              fontSize={11}
              tickFormatter={kind === "tokens" ? (v) => humanTokens(Number(v)) : undefined}
            />
            <YAxis type="category" dataKey="label" fontSize={11} width={130} />
            <RTooltip
              formatter={(v: number) => (kind === "tokens" ? humanTokens(v) : v)}
            />
            <Bar
              dataKey={kind}
              fill={chartColors[kind === "sessions" ? 0 : 1]}
              onClick={(d: any) => {
                const row = d?.payload ?? d;
                setPrefill(row.cwd);
                setView("project");
                nav(`/${row.provider ?? "codex"}/sessions`);
              }}
              cursor="pointer"
            />
          </BarChart>
        </ResponsiveContainer>
      </CardContent>
    </Card>
  );
}

function ModelDistCard({ data }: { data: ModelStat[] }) {
  const totalSessions = useMemo(() => data.reduce((a, b) => a + b.sessions, 0), [data]);
  const labelFor = (m: ModelStat) => modelStatLabel(m);
  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-sm">模型与推理强度</CardTitle>
      </CardHeader>
      <CardContent>
        {data.length === 0 ? (
          <div className="flex h-48 items-center justify-center text-xs text-muted-foreground">
            无数据
          </div>
        ) : (
          <div className="grid min-w-0 grid-cols-1 items-center gap-4 sm:grid-cols-[160px_minmax(0,1fr)]">
            <div className="h-48 sm:h-40">
              <ResponsiveContainer width="100%" height="100%">
                <PieChart>
                  <Pie
                    data={data.map((m, i) => ({
                      name: labelFor(m),
                      value: m.sessions,
                      fill: chartColors[i % chartColors.length],
                    }))}
                    dataKey="value"
                    nameKey="name"
                    innerRadius={38}
                    outerRadius={70}
                    paddingAngle={2}
                  >
                    {data.map((_, i) => (
                      <Cell key={i} fill={chartColors[i % chartColors.length]} />
                    ))}
                  </Pie>
                  <RTooltip
                    formatter={(v: number, name: string) => [`${v} 条`, name]}
                  />
                </PieChart>
              </ResponsiveContainer>
            </div>
            <ul className="min-w-0 space-y-3 text-xs">
              {data.map((m, i) => {
                const pct = totalSessions ? (m.sessions / totalSessions) * 100 : 0;
                const model = m.model || "(未标注)";
                const effort = m.reasoning_effort;
                const provider = m.provider ? providerLabel(m.provider) : null;
                return (
                  <li key={`${m.model || "(empty)"}:${m.reasoning_effort || "(empty)"}:${i}`} className="min-w-0 space-y-1.5">
                    <div className="grid min-w-0 grid-cols-[max-content_minmax(0,1fr)] gap-2">
                      <span
                        className="mt-1 h-2.5 w-2.5 shrink-0 rounded-sm"
                        style={{ background: chartColors[i % chartColors.length] }}
                      />
                      <div className="min-w-0 space-y-0.5">
                        <div className="flex min-w-0 items-center justify-between gap-2">
                          <span className="min-w-0 truncate font-medium" title={model}>
                            {provider ? `[${provider}] ${model}` : model}
                          </span>
                          <span className="shrink-0 tabular-nums text-muted-foreground">
                            {m.sessions} 条
                          </span>
                        </div>
                        <div className="flex min-w-0 items-center justify-between gap-2 text-muted-foreground">
                          {effort ? (
                            <span className="min-w-0 truncate" title={effort}>
                              {effort}
                            </span>
                          ) : (
                            <span />
                          )}
                          <span className="shrink-0 tabular-nums">
                            {humanTokens(m.tokens)} token
                          </span>
                        </div>
                      </div>
                    </div>
                    <div className="h-1.5 w-full overflow-hidden rounded-full bg-muted">
                      <div
                        className="h-full rounded-full"
                        style={{
                          width: `${pct}%`,
                          background: chartColors[i % chartColors.length],
                        }}
                      />
                    </div>
                  </li>
                );
              })}
            </ul>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

function modelStatLabel(m: ModelStat): string {
  const model = m.model || "(未标注)";
  const label = m.provider ? `[${providerLabel(m.provider)}] ${model}` : model;
  return m.reasoning_effort ? `${label} · ${m.reasoning_effort}` : label;
}

function providerLabel(provider: string): string {
  if (provider === "claude") return "Claude";
  if (provider === "gemini") return "Gemini";
  return "Codex";
}

function HeatmapCard({ data }: { data: number[][] }) {
  const dayNames = ["日", "一", "二", "三", "四", "五", "六"];

  // 按最大值分 5 档（0 / 1-25% / 25-50% / 50-75% / 75-100%），更贴近 GitHub 的离散色阶
  const max = useMemo(() => Math.max(1, ...data.flat()), [data]);
  const levelClasses = [
    "bg-muted border border-border/60", // 0
    "bg-emerald-200 dark:bg-emerald-900/70",
    "bg-emerald-400 dark:bg-emerald-700/80",
    "bg-emerald-500 dark:bg-emerald-500/90",
    "bg-emerald-600 dark:bg-emerald-400",
  ];
  const levelFor = (v: number) => {
    if (v <= 0) return 0;
    const r = v / max;
    if (r < 0.25) return 1;
    if (r < 0.5) return 2;
    if (r < 0.75) return 3;
    return 4;
  };

  return (
    <Card className="lg:col-span-2">
      <CardHeader className="pb-2">
        <div className="flex items-center justify-between gap-2">
          <CardTitle className="text-sm">活跃度热力图</CardTitle>
          <div className="flex items-center gap-1.5 text-[11px] text-muted-foreground">
            <span>少</span>
            {levelClasses.map((c, i) => (
              <span key={i} className={cn("h-2.5 w-2.5 rounded-[3px]", c)} />
            ))}
            <span>多</span>
          </div>
        </div>
      </CardHeader>
      <CardContent>
        <div className="overflow-x-auto">
          <div className="inline-block min-w-max">
            {/* 小时坐标 */}
            <div className="mb-1.5 ml-6 flex gap-[3px]">
              {Array.from({ length: 24 }).map((_, h) => (
                <div
                  key={h}
                  className="flex w-[14px] justify-center text-[10px] text-muted-foreground"
                >
                  {h % 3 === 0 ? h : ""}
                </div>
              ))}
            </div>
            {/* 方阵 */}
            <div className="flex flex-col gap-[3px]">
              {data.map((row, d) => (
                <div key={d} className="flex items-center gap-[3px]">
                  <div className="w-5 pr-1 text-right text-[10px] text-muted-foreground">
                    {dayNames[d]}
                  </div>
                  {row.map((v, h) => {
                    const lv = levelFor(v);
                    return (
                      <div
                        key={h}
                        title={`周${dayNames[d]} ${h}:00 — ${v} 次`}
                        className={cn(
                          "h-[14px] w-[14px] rounded-[3px] transition-transform hover:scale-110",
                          levelClasses[lv],
                        )}
                      />
                    );
                  })}
                </div>
              ))}
            </div>
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

function formatBucket(ts: number, bucket: Bucket): string {
  const d = new Date(ts * 1000);
  if (bucket === "week") {
    const isoWeek = getIsoWeek(d);
    return `Wk${isoWeek}·${format(d, "MM-dd")}`;
  }
  return format(d, "MM-dd");
}

function getIsoWeek(d: Date): number {
  const target = new Date(d.valueOf());
  const dayNr = (d.getDay() + 6) % 7;
  target.setDate(target.getDate() - dayNr + 3);
  const firstThursday = new Date(target.getFullYear(), 0, 4);
  const diff = (target.getTime() - firstThursday.getTime()) / 86400000;
  return 1 + Math.round(diff / 7);
}

// 意外未使用的导入提示静默
export const _utils = { relativeTime };
