import * as React from "react";
import { ChevronDown, ChevronLeft, ChevronRight } from "lucide-react";
import { DayPicker, type DayButton } from "react-day-picker";

import { Button, buttonVariants } from "@/components/ui/button";
import { cn } from "@/lib/utils";

function Calendar({
  className,
  classNames,
  showOutsideDays = true,
  captionLayout = "label",
  components,
  ...props
}: React.ComponentProps<typeof DayPicker>) {
  return (
    <DayPicker
      showOutsideDays={showOutsideDays}
      captionLayout={captionLayout}
      className={cn("relative p-3", className)}
      classNames={{
        months: "flex flex-col gap-4 sm:flex-row",
        month: "space-y-4",
        nav: "flex items-center",
        button_previous: cn(
          buttonVariants({ variant: "ghost" }),
          "absolute left-3 top-3 h-8 w-8 bg-transparent p-0 opacity-70 hover:opacity-100",
        ),
        button_next: cn(
          buttonVariants({ variant: "ghost" }),
          "absolute right-3 top-3 h-8 w-8 bg-transparent p-0 opacity-70 hover:opacity-100",
        ),
        month_caption: "flex h-8 items-center justify-center px-8",
        dropdowns: "flex h-8 items-center justify-center gap-1.5 text-sm font-medium",
        dropdown_root:
          "relative rounded-md border border-input shadow-sm focus-within:ring-1 focus-within:ring-ring data-[disabled=true]:opacity-50",
        dropdown: "absolute inset-0 cursor-pointer bg-popover opacity-0 disabled:cursor-not-allowed",
        caption_label: cn(
          "font-medium",
          captionLayout === "label"
            ? "text-sm"
            : "flex h-8 items-center gap-1 rounded-md px-2 text-sm [&>svg]:h-3.5 [&>svg]:w-3.5 [&>svg]:text-muted-foreground",
        ),
        month_grid: "w-full border-collapse",
        weekdays: "flex",
        weekday:
          "w-8 rounded-md text-center text-[0.8rem] font-normal text-muted-foreground",
        week: "mt-1 flex w-full",
        day: "relative h-8 w-8 p-0 text-center text-sm",
        day_button: "h-8 w-8 p-0 font-normal",
        outside: "text-muted-foreground",
        disabled: "text-muted-foreground opacity-40",
        hidden: "invisible",
        ...classNames,
      }}
      components={{
        Chevron: ({ className, orientation, ...chevronProps }) => {
          if (orientation === "left") {
            return <ChevronLeft className={cn("h-4 w-4", className)} {...chevronProps} />;
          }
          if (orientation === "right") {
            return <ChevronRight className={cn("h-4 w-4", className)} {...chevronProps} />;
          }
          return <ChevronDown className={cn("h-4 w-4", className)} {...chevronProps} />;
        },
        DayButton: CalendarDayButton,
        ...components,
      }}
      {...props}
    />
  );
}

function CalendarDayButton({
  className,
  day,
  modifiers,
  ...props
}: React.ComponentProps<typeof DayButton>) {
  const ref = React.useRef<HTMLButtonElement>(null);

  React.useEffect(() => {
    if (modifiers.focused) ref.current?.focus();
  }, [modifiers.focused]);

  return (
    <Button
      ref={ref}
      variant="ghost"
      size="icon"
      data-selected-single={
        modifiers.selected &&
        !modifiers.range_start &&
        !modifiers.range_end &&
        !modifiers.range_middle
      }
      data-range-start={modifiers.range_start}
      data-range-end={modifiers.range_end}
      data-range-middle={modifiers.range_middle}
      className={cn(
        "h-8 w-8 p-0 font-normal",
        "data-[selected-single=true]:bg-primary data-[selected-single=true]:text-primary-foreground",
        "data-[range-start=true]:bg-primary data-[range-start=true]:text-primary-foreground",
        "data-[range-end=true]:bg-primary data-[range-end=true]:text-primary-foreground",
        "data-[range-middle=true]:rounded-none data-[range-middle=true]:bg-accent",
        modifiers.today && !modifiers.selected && "border border-primary/40 bg-accent",
        modifiers.outside && "text-muted-foreground opacity-50",
        modifiers.disabled && "text-muted-foreground opacity-40",
        className,
      )}
      {...props}
    />
  );
}

export { Calendar, CalendarDayButton };
