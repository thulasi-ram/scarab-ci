// Connected apps and OAuth consent, as the UI reasons about them. Pure — no
// DOM, no fetch.

/** The authorization request's parameters from `/oauth/authorize`'s query,
 * or why the page cannot go on. Only the names the server takes are kept. */
export function authorizationParams(
  search: string,
):
  | {
      ok: true;
      params: {
        response_type: string;
        client_id: string;
        redirect_uri: string;
        code_challenge: string;
        code_challenge_method: string;
        resource: string;
        state?: string;
        scope?: string;
      };
    }
  | { ok: false; error: string } {
  const q = new URLSearchParams(search);
  const required = [
    "response_type",
    "client_id",
    "redirect_uri",
    "code_challenge",
    "code_challenge_method",
    "resource",
  ] as const;
  const missing = required.filter((k) => !q.get(k));
  if (missing.length > 0) {
    return { ok: false, error: `The app's request is missing ${missing.join(", ")}.` };
  }
  const get = (k: string) => q.get(k) ?? "";
  return {
    ok: true,
    params: {
      response_type: get("response_type"),
      client_id: get("client_id"),
      redirect_uri: get("redirect_uri"),
      code_challenge: get("code_challenge"),
      code_challenge_method: get("code_challenge_method"),
      resource: get("resource"),
      ...(q.get("state") !== null ? { state: get("state") } : {}),
      ...(q.get("scope") ? { scope: get("scope") } : {}),
    },
  };
}

/** Where a denial sends the browser: the client's redirect URI with
 * `error=access_denied` and the request's `state`. */
export function deniedRedirect(redirectUri: string, state?: string): string {
  const url = new URL(redirectUri);
  url.searchParams.set("error", "access_denied");
  if (state !== undefined) url.searchParams.set("state", state);
  return url.toString();
}

/** Whether a connected app can still act for you at `now`. */
export function connectedAppState(
  app: { revoked_at?: number | null; ends_at: number },
  now: number,
): "connected" | "disconnected" | "ended" {
  if (app.revoked_at != null) return "disconnected";
  return app.ends_at <= now ? "ended" : "connected";
}

/** The endpoint a connected app is for, as a person reads it: the part of
 * its resource URL after `/plugin-installations/`. */
export function endpointLabel(resource: string): string {
  const i = resource.indexOf("/plugin-installations/");
  return i < 0 ? resource : resource.slice(i + "/plugin-installations/".length);
}
