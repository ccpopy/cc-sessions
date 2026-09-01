import * as React from "react";
import { ChevronDown, ChevronLeft, ChevronRight } from "lucide-react";
import {
  DayPicker,
  type DayButton,
  type DropdownProps,
} from "react-day-picker";

import { Button, buttonVariants } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
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
        month: "relative",
        nav: "pointer-events-none absolute inset-x-0 top-0 flex h-8 items-center justify-between",
        button_previous: cn(
          buttonVariants({ variant: "ghost" }),
          "pointer-events-auto h-8 w-8 bg-transparent p-0 text-muted-foreground shadow-none hover:text-foreground aria-disabled:pointer-events-none aria-disabled:opacity-30",
        ),
        button_next: cn(
          buttonVariants({ variant: "ghost" }),
          "pointer-events-auto h-8 w-8 bg-transparent p-0 text-muted-foreground shadow-none hover:text-foreground aria-disabled:pointer-events-none aria-disabled:opacity-30",
        ),
        month_caption: "flex h-8 items-center justify-center px-10",
        dropdowns: "flex h-8 items-center justify-center gap-1 text-sm font-medium",
        months_dropdown: "w-[4.5rem]",
        years_dropdown: "w-[5rem]",
        caption_label: cn(
          "font-medium",
          captionLayout === "label"
            ? "text-sm"
            : "flex h-8 items-center gap-1 rounded-md px-2 text-sm [&>svg]:h-3.5 [&>svg]:w-3.5 [&>svg]:text-muted-foreground",
        ),
        month_grid: "mt-4 w-full border-collapse",
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
        Dropdown: CalendarDropdown,
        DayButton: CalendarDayButton,
        ...components,
      }}
      {...props}
    />
  );
}

function CalendarDropdown({
  options,
  value,
  onChange,
  disabled,
  className,
  "aria-label": ariaLabel,
}: DropdownProps) {
  const [open, setOpen] = React.useState(false);
  const selectedValue = value === undefined ? undefined : String(value);

  return (
    <Select
      open={open}
      onOpenChange={setOpen}
      value={selectedValue}
      disabled={disabled}
      onValueChange={(nextValue) => {
        setOpen(false);
        onChange?.({ target: { value: nextValue } } as React.ChangeEvent<HTMLSelectElement>);
      }}
    >
      <SelectTrigger
        aria-label={ariaLabel}
        className={cn(
          "h-8 border-0 bg-muted/55 px-2.5 py-0 text-xs font-medium shadow-none",
          "hover:bg-muted focus:ring-0 focus:ring-offset-0 data-[state=open]:bg-accent",
          "[&>svg]:h-3.5 [&>svg]:w-3.5 [&>svg]:text-muted-foreground",
          className,
        )}
      >
        <SelectValue />
      </SelectTrigger>
      <SelectContent
        position="popper"
        align="start"
        className={cn(
          "z-[60] !max-h-64 min-w-[var(--radix-select-trigger-width)]",
          "[&_[data-radix-select-viewport]]:!h-auto",
        )}
      >
        {options?.map((option) => (
          <SelectItem
            key={option.value}
            value={String(option.value)}
            disabled={option.disabled}
            className="text-xs"
          >
            {option.label}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
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
