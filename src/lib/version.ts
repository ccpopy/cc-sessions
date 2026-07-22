export function normalizeVersion(raw: string): string {
  return raw.trim().replace(/^v/i, "");
}

export function compareVersions(a: string, b: string): number {
  const left = parseVersion(a);
  const right = parseVersion(b);
  for (let index = 0; index < Math.max(left.length, right.length); index += 1) {
    const leftPart = left[index] ?? 0;
    const rightPart = right[index] ?? 0;
    if (leftPart !== rightPart) return leftPart > rightPart ? 1 : -1;
  }
  return 0;
}

function parseVersion(version: string): number[] {
  return normalizeVersion(version)
    .split(/[.-]/)
    .map((part) => Number.parseInt(part, 10))
    .filter((part) => Number.isFinite(part));
}
