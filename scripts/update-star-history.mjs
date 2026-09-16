// GitHub's paginated, aggregate history contains no stargazer identities.
// https://docs.github.com/en/rest/activity/starring#get-repository-star-history
import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

const DAY = 86400000;

export function renderStarHistory(weeks, createdAt, updatedAt) {
  if (weeks.some((week) => !Number.isFinite(week.week) || week.days?.length !== 7
    || week.days.some((count) => !Number.isInteger(count) || count < 0))) {
    throw new Error("Invalid GitHub star history; the existing chart was not changed.");
  }
  const days = weeks.flatMap((week) => week.days.map((count, index) => ({
    time: week.week * 1000 + index * DAY, count,
  }))).sort((a, b) => a.time - b.time);
  const total = days.reduce((sum, day) => sum + day.count, 0);
  const created = new Date(createdAt);
  const updated = new Date(updatedAt);
  const start = Date.UTC(created.getUTCFullYear(), created.getUTCMonth(), 1);
  const end = Date.UTC(updated.getUTCFullYear(), updated.getUTCMonth(), updated.getUTCDate() + 1);
  const roughStep = Math.max(1, total / 4);
  const magnitude = 10 ** Math.floor(Math.log10(roughStep));
  const step = [1, 2, 5, 10].find((value) => value * magnitude >= roughStep) * magnitude;
  const maximum = Math.max(step, Math.ceil(total / step) * step);
  const x = (time) => 66 + (time - start) / (end - start) * 866;
  const y = (count) => 306 - count / maximum * 236;
  const number = (value) => String(Number(value.toFixed(2)));
  const month = (time) => new Date(time).toISOString().slice(0, 7);
  const grid = [];
  for (let count = 0; count <= maximum; count += step) {
    const position = number(y(count));
    grid.push(`  <line class="grid" x1="66" y1="${position}" x2="932" y2="${position}"/>
  <text class="axis" x="54" y="${number(y(count) + 4)}" text-anchor="end">${count}</text>`);
  }
  const months = (updated.getUTCFullYear() - created.getUTCFullYear()) * 12
    + updated.getUTCMonth() - created.getUTCMonth();
  const ticks = [];
  for (let index = 0; index <= months; index += Math.max(1, Math.ceil(months / 7))) {
    const time = Date.UTC(created.getUTCFullYear(), created.getUTCMonth() + index, 1);
    const position = number(x(time));
    const anchor = index === 0 ? "start" : x(time) > 890 ? "end" : "middle";
    ticks.push(`  <line class="tick" x1="${position}" y1="306" x2="${position}" y2="312"/>
  <text class="axis" x="${position}" y="336" text-anchor="${anchor}">${month(time)}</text>`);
  }
  let cumulative = 0;
  let lastX = "66";
  let lastY = "306";
  let line = "M 66 306";
  for (const day of days) {
    if (day.count === 0) continue;
    cumulative += day.count;
    lastX = number(x(day.time));
    lastY = number(y(cumulative));
    line += ` H ${lastX} V ${lastY}`;
  }
  line += " H 932";
  return `<svg xmlns="http://www.w3.org/2000/svg" width="960" height="360" viewBox="0 0 960 360" role="img" aria-labelledby="title desc">
  <title id="title">CC Sessions Star History</title>
  <desc id="desc">CC Sessions 从 ${month(start)} 到 ${month(end - DAY)} 的 GitHub Star 累计趋势，共 ${total} 个 Star。</desc>
  <defs>
    <linearGradient id="area" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0%" stop-color="#2f81f7" stop-opacity="0.34"/>
      <stop offset="100%" stop-color="#2f81f7" stop-opacity="0.03"/>
    </linearGradient>
  </defs>
  <style>
    .bg { fill: #ffffff; stroke: #d0d7de; }
    .title { fill: #1f2328; font: 600 20px -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }
    .meta { fill: #656d76; font: 13px -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }
    .axis { fill: #656d76; font: 12px -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }
    .grid { stroke: #d8dee4; stroke-width: 1; }
    .tick { stroke: #8c959f; stroke-width: 1; }
    .area { fill: url(#area); }
    .line { fill: none; stroke: #0969da; stroke-width: 3; stroke-linejoin: round; stroke-linecap: round; }
    .dot { fill: #0969da; stroke: #ffffff; stroke-width: 3; }
    @media (prefers-color-scheme: dark) {
      .bg { fill: #0d1117; stroke: #30363d; }
      .title { fill: #f0f6fc; }
      .meta, .axis { fill: #8b949e; }
      .grid { stroke: #30363d; }
      .tick { stroke: #6e7681; }
      .line { stroke: #58a6ff; }
      .dot { fill: #58a6ff; stroke: #0d1117; }
    }
  </style>
  <rect class="bg" x="0.5" y="0.5" width="959" height="359" rx="12"/>
  <text class="title" x="66" y="32">CC Sessions Star History</text>
  <text class="meta" x="66" y="53">${total} stars · 数据更新于 ${updated.toISOString().slice(0, 10)} · 来源 GitHub Star History</text>
${grid.join("\n")}
${ticks.join("\n")}
  <path class="area" d="${line} L 932 306 L 66 306 Z"/>
  <path class="line" d="${line}"/>
  <circle class="dot" cx="${lastX}" cy="${lastY}" r="5"/>
</svg>
`;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const repository = process.env.GITHUB_REPOSITORY || "ccpopy/cc-sessions";
  const api = (endpoint, ...flags) => JSON.parse(execFileSync("gh", [
    "api", endpoint, "-H", "Accept: application/vnd.github+json",
    "-H", "X-GitHub-Api-Version: 2026-03-10", ...flags,
  ], { encoding: "utf8", windowsHide: true }));
  const { created_at } = api(`repos/${repository}`);
  const weeks = api(`repos/${repository}/stargazers/history?per_page=30`, "--paginate", "--slurp").flat();
  const svg = renderStarHistory(weeks, created_at, new Date());
  writeFileSync(new URL("../img/star-history.svg", import.meta.url), svg);
  console.log(`Updated Star history for ${repository}.`);
}
