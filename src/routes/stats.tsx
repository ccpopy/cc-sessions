import { TopBar } from "@/components/TopBar";
import { StatsDashboard } from "@/components/StatsDashboard";
import { ScrollArea } from "@/components/ui/scroll-area";

export default function StatsRoute() {
  return (
    <>
      <TopBar title="统计" />
      <ScrollArea className="flex-1">
        <StatsDashboard />
      </ScrollArea>
    </>
  );
}
