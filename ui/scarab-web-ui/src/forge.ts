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
