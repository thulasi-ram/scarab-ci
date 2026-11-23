// Forge web links for a commit / PR, built from the repo's forge web base
// (`ProjectDto.repo_url`, e.g. `https://github.com/owner/name`) — resolved
// server-side from the repo's ForgeConnection, so GitHub / Forgejo / GHES all
// link correctly (no host assumption here).

export function forgeCommitUrl(repoUrl?: string | null, sha?: string | null): string | null {
  if (!repoUrl || !sha) return null;
  return `${repoUrl}/commit/${sha}`;
}

export function forgePrUrl(repoUrl?: string | null, pr?: number | null): string | null {
  if (!repoUrl || pr == null) return null;
  return `${repoUrl}/pull/${pr}`;
}

/** A file (or folder) at a commit. The repo base carries no forge kind, so the
 * host decides: GitHub (`github.com`, or a GitHub Enterprise host named
 * `github.…`) writes `/blob/<sha>/<path>`, which also opens a folder; any
 * other host is taken as Forgejo, which writes `/src/commit/<sha>/<path>`. */
export function forgeFileUrl(repoUrl?: string | null, sha?: string | null, path?: string | null): string | null {
  if (!repoUrl || !sha || !path) return null;
  let host = "";
  try {
    host = new URL(repoUrl).hostname;
  } catch {
    return null;
  }
  const file = path.replace(/^\/+/, "").split("/").map(encodeURIComponent).join("/");
  return /^github\./.test(host)
    ? `${repoUrl}/blob/${sha}/${file}`
    : `${repoUrl}/src/commit/${sha}/${file}`;
}
