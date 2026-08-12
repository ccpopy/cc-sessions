import type { ReactNode, RefObject } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";

type Props<Row> = {
  rows: readonly Row[];
  scrollElementRef: RefObject<HTMLDivElement | null>;
  estimateSize: (row: Row) => number;
  getRowKey: (row: Row) => string;
  renderRow: (row: Row) => ReactNode;
  overscan?: number;
};

export function VirtualList<Row>({
  rows,
  scrollElementRef,
  estimateSize,
  getRowKey,
  renderRow,
  overscan = 8,
}: Props<Row>) {
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollElementRef.current,
    estimateSize: (index) => estimateSize(rows[index]),
    getItemKey: (index) => getRowKey(rows[index]),
    gap: 12,
    overscan,
    paddingStart: 20,
    paddingEnd: 20,
  });

  return (
    <div
      className="relative min-w-0 w-full"
      style={{ height: `${virtualizer.getTotalSize()}px` }}
    >
      {virtualizer.getVirtualItems().map((virtualRow) => {
        const row = rows[virtualRow.index];
        return (
          <div
            key={virtualRow.key}
            ref={virtualizer.measureElement}
            data-index={virtualRow.index}
            className="absolute left-0 top-0 min-w-0 w-full px-6"
            style={{ transform: `translateY(${virtualRow.start}px)` }}
          >
            {renderRow(row)}
          </div>
        );
      })}
    </div>
  );
}
