import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

export const platforms = ["windows", "linux", "macos-arm64", "macos-intel"];

export function validateRelease({ needs, sha, tagSha, version, proofs, assets, draft }) {
  for (const job of ["verify", "build"]) {
    if (needs[job]?.result !== "success") throw new Error(`Required job did not pass: ${job}`);
  }
  if (!sha || sha !== tagSha) throw new Error("Tag does not resolve to the verified commit");
  if (!draft) throw new Error("Release must remain a draft until this gate passes");
  for (const platform of platforms) {
    const proof = proofs.filter((p) => p.platform === platform);
    if (proof.length !== 1 || proof[0].sha !== sha || proof[0].version !== version) {
      throw new Error(`Missing or mismatched build proof: ${platform}`);
    }
  }
  const expected = [
    ...platforms.map((p) => `cc-sessions-cli-v${version}-${p}.zip`),
    `cc-session-manager-portable-v${version}-windows.exe`,
    `cc-session-manager-portable-v${version}-windows.zip`,
    `CC.Sessions_${version}_x64-setup.exe`,
    `CC.Sessions_${version}_amd64.AppImage`,
    `CC.Sessions_${version}_aarch64.dmg`, `CC.Sessions_${version}_x64.dmg`,
  ];
  for (const name of expected) {
    if (!assets.some((a) => a.name === name && a.size > 0)) throw new Error(`Missing release asset: ${name}`);
  }
}

// The only publish command in the workflow is executed after these checks succeed.
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const tag = process.env.GITHUB_REF_NAME;
  const sha = process.env.GITHUB_SHA;
  const gh = (...args) => execFileSync("gh", args, { encoding: "utf8", windowsHide: true });
  const release = JSON.parse(gh("release", "view", tag, "--json", "isDraft,assets"));
  const proofs = platforms.map((platform) => JSON.parse(readFileSync(`output/release-gate/build-proof-${platform}.json`, "utf8")));
  validateRelease({ needs: JSON.parse(process.env.RELEASE_NEEDS), sha,
    tagSha: execFileSync("git", ["rev-parse", `${tag}^{commit}`], { encoding: "utf8" }).trim(),
    version: tag.slice(1), proofs, assets: release.assets, draft: release.isDraft });
  gh("release", "edit", tag, "--draft=false", "--prerelease=false", "--verify-tag");
}
