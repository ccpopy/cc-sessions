import { useMemo, useState } from "react";
import { format } from "date-fns";
import { zhCN } from "date-fns/locale";
import { CalendarDays } from "lucide-react";
import type { Matcher } from "react-day-picker";

import { Button } from "@/components/ui/button";
import { Calendar } from "@/components/ui/calendar";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import { formatLocalDate, parseLocalDate } from "@/lib/exportDateRange";
import { cn } from "@/lib/utils";

type DatePickerProps = {
  id?: string;
  value: string;
  onChange: (value: string) => void;
  placeholder?: string;
  minDate?: Date;
  maxDate?: Date;
  ariaLabel?: string;
  className?: string;
};

function DatePicker({
  id,
  value,
  onChange,
  placeholder = "选择日期",
  minDate,
  maxDate,
  ariaLabel,
  className,
}: DatePickerProps) {
  const [open, setOpen] = useState(false);
  const selectedDate = useMemo(() => parseLocalDate(value), [value]);
  const minimumDate = normalizeDate(minDate);
  const maximumDate = normalizeDate(maxDate);
  const disabledDates = useMemo<Matcher[]>(() => {
    const matchers: Matcher[] = [];
    if (minimumDate) matchers.push({ before: minimumDate });
    if (maximumDate) matchers.push({ after: maximumDate });
    return matchers;
  }, [minimumDate?.getTime(), maximumDate?.getTime()]);

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          id={id}
          type="button"
          variant="outline"
          aria-label={ariaLabel}
          aria-expanded={open}
          className={cn(
            "w-40 justify-start px-3 text-left text-xs font-normal",
            !selectedDate && "text-muted-foreground",
            className,
          )}
        >
          <CalendarDays className="h-3.5 w-3.5" />
          {selectedDate
            ? format(selectedDate, "yyyy年M月d日", { locale: zhCN })
            : placeholder}
        </Button>
      </PopoverTrigger>
      <PopoverContent align="start" className="w-auto p-0">
        <Calendar
          mode="single"
          captionLayout="dropdown"
          navLayout="after"
          reverseYears
          selected={selectedDate}
          defaultMonth={selectedDate ?? maximumDate ?? new Date()}
          onSelect={(date) => {
            onChange(date ? formatLocalDate(date) : "");
            if (date) setOpen(false);
          }}
          disabled={disabledDates}
          startMonth={minimumDate}
          endMonth={maximumDate}
          locale={zhCN}
          autoFocus
        />
        {(minimumDate || maximumDate) && (
          <div className="border-t px-3 py-2 text-[11px] text-muted-foreground">
            {dateBoundaryHint(minimumDate, maximumDate)}
          </div>
        )}
      </PopoverContent>
    </Popover>
  );
}

function normalizeDate(value?: Date): Date | undefined {
  if (!value || Number.isNaN(value.getTime())) return undefined;
  return new Date(value.getFullYear(), value.getMonth(), value.getDate());
}

function dateBoundaryHint(minDate?: Date, maxDate?: Date): string {
  if (minDate && maxDate) {
    return `可选范围：${format(minDate, "yyyy-MM-dd")} 至 ${format(maxDate, "yyyy-MM-dd")}`;
  }
  if (minDate) return `最早可选：${format(minDate, "yyyy-MM-dd")}`;
  return `最晚可选：${format(maxDate as Date, "yyyy-MM-dd")}`;
}

export { DatePicker };
