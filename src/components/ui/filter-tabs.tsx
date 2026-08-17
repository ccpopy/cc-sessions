import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";

export function FilterTabs<T extends string>({
  value,
  options,
  onChange,
  label,
}: {
  value: T;
  options: { value: T; label: string }[];
  onChange: (value: T) => void;
  label?: string;
}) {
  return (
    <Tabs value={value} onValueChange={(next) => onChange(next as T)}>
      <TabsList className="h-9 bg-card/75 p-0.5 ring-1 ring-inset ring-border/70">
        {label && (
          <span className="select-none pl-2 pr-1.5 text-[10px] font-semibold uppercase tracking-[0.14em] text-muted-foreground/65">
            {label}
          </span>
        )}
        {options.map((item) => (
          <TabsTrigger
            key={item.value}
            value={item.value}
            className="h-8 px-2 text-xs data-[state=active]:bg-primary data-[state=active]:text-primary-foreground sm:px-2.5"
          >
            {item.label}
          </TabsTrigger>
        ))}
      </TabsList>
    </Tabs>
  );
}