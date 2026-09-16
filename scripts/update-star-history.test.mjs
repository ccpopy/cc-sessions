import assert from "node:assert/strict";
import test from "node:test";
import { renderStarHistory } from "./update-star-history.mjs";

test("newest-first API weeks render chronologically with the complete star count", () => {
  const svg = renderStarHistory([
    { week: Date.parse("2026-09-13T00:00:00Z") / 1000, days: [1, 2, 0, 0, 0, 0, 0] },
    { week: Date.parse("2026-09-06T00:00:00Z") / 1000, days: [3, 0, 0, 0, 0, 0, 0] },
  ], "2026-09-06T00:00:00Z", "2026-09-16T10:00:00Z");
  assert.match(svg, /6 stars · 数据更新于 2026-09-16/);
  const line = svg.match(/class="line" d="([^"]+)"/)[1];
  const points = [...line.matchAll(/H ([\d.]+) V ([\d.]+)/g)].map((point) => point.slice(1).map(Number));
  assert.equal(points.length, 3);
  assert.ok(points[0][0] < points[1][0] && points[1][0] < points[2][0]);
  assert.ok(points[0][1] > points[1][1] && points[1][1] > points[2][1]);
});

test("a repository without stars has a finite, flat chart", () => {
  const svg = renderStarHistory([], "2026-09-16T00:00:00Z", "2026-09-16T10:00:00Z");
  assert.match(svg, /0 stars/);
  assert.match(svg, /class="line" d="M 66 306 H 932"/);
  assert.doesNotMatch(svg, /NaN|Infinity/);
});
