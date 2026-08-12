import { useEffect, useRef } from "react";
import { BarChart, LineChart, PieChart } from "echarts/charts";
import { GridComponent, TooltipComponent } from "echarts/components";
import { init, use, type ECharts, type EChartsCoreOption } from "echarts/core";
import { CanvasRenderer } from "echarts/renderers";

import { cn } from "@/lib/utils";

use([BarChart, LineChart, PieChart, GridComponent, TooltipComponent, CanvasRenderer]);

export function EChart({ option, className }: { option: EChartsCoreOption; className?: string }) {
  const hostRef = useRef<HTMLDivElement>(null);
  const chartRef = useRef<ECharts | null>(null);

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;

    const chart = init(host, undefined, { renderer: "canvas" });
    chartRef.current = chart;
    chart.setOption(option, { notMerge: true });

    const observer = new ResizeObserver(() => chart.resize());
    observer.observe(host);

    return () => {
      observer.disconnect();
      chart.dispose();
      chartRef.current = null;
    };
  }, []);

  useEffect(() => {
    chartRef.current?.setOption(option, { notMerge: true, lazyUpdate: true });
  }, [option]);

  return <div ref={hostRef} className={cn("h-full w-full", className)} />;
}
