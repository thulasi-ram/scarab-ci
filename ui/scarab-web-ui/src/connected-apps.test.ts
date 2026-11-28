import { describe, it, expect } from "vitest";
import { authorizationParams, connectedAppState, deniedRedirect, endpointLabel } from "./connected-apps";

describe("authorizationParams", () => {
  it("keeps the request's parameters and names what is missing", () => {
    const ok = authorizationParams(
      "?response_type=code&client_id=https%3A%2F%2Fclaude.ai%2Fm&redirect_uri=https%3A%2F%2Fclaude.ai%2Fcb" +
        "&code_challenge=abc&code_challenge_method=S256&resource=https%3A%2F%2Fci%2Fx&state=s1&extra=1",
    );
    expect(ok.ok).toBe(true);
    if (ok.ok) {
      expect(ok.params.client_id).toBe("https://claude.ai/m");
      expect(ok.params.state).toBe("s1");
      expect("extra" in ok.params).toBe(false);
    }
    const bad = authorizationParams("?response_type=code");
    expect(bad.ok).toBe(false);
  });
});

describe("deniedRedirect", () => {
  it("adds access_denied and the state", () => {
    expect(deniedRedirect("https://claude.ai/api/mcp/auth_callback", "s1")).toBe(
      "https://claude.ai/api/mcp/auth_callback?error=access_denied&state=s1",
    );
  });
});

describe("connectedAppState and endpointLabel", () => {
  it("says whether the app can still act", () => {
    expect(connectedAppState({ ends_at: 10 }, 5)).toBe("connected");
    expect(connectedAppState({ ends_at: 10 }, 10)).toBe("ended");
    expect(connectedAppState({ ends_at: 10, revoked_at: 3 }, 5)).toBe("disconnected");
    expect(endpointLabel("https://ci/v1/orgs/acme/plugin-installations/0a1b/endpoints/mcp")).toBe(
      "0a1b/endpoints/mcp",
    );
  });
});
