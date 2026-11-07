// The debug shell's WebSocket (git-bug aeba20a). A browser cannot send
// `x-csrf-token` on a handshake, so the token rides as a subprotocol; and a
// refused handshake says nothing, so a plain GET of the same URL goes first,
// through the step-up retry, to open the window.
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { openShellSocket } from "./api/client";

const fetches: { url: string; init: RequestInit | undefined }[] = [];
const sockets: { url: string; protocols: string[] }[] = [];
let probeStatus = 400;

class FakeSocket {
  constructor(url: string, protocols: string[]) {
    sockets.push({ url, protocols });
  }
  close() {}
}

beforeEach(() => {
  fetches.length = 0;
  sockets.length = 0;
  probeStatus = 400;
  vi.stubGlobal("document", { cookie: "theme=dark; scarab_csrf=tok-123" });
  vi.stubGlobal("location", { protocol: "https:", host: "ci.example.com" });
  vi.stubGlobal("WebSocket", FakeSocket);
  vi.stubGlobal("fetch", async (url: string, init?: RequestInit) => {
    fetches.push({ url, init });
    return new Response("connection header did not include 'upgrade'", { status: probeStatus });
  });
});

afterEach(() => vi.unstubAllGlobals());

describe("openShellSocket", () => {
  it("checks the URL first, then offers the CSRF token as a subprotocol", async () => {
    const opened = await openShellSocket("r 1", "build", "debug-pod");
    expect(typeof opened).not.toBe("string");
    expect(fetches.map((f) => f.url)).toEqual(["/v1/runs/r%201/steps/build/debug-pod"]);
    expect((fetches[0]!.init?.headers as Record<string, string>)["x-csrf-token"]).toBe("tok-123");
    expect(sockets).toEqual([
      {
        url: "wss://ci.example.com/v1/runs/r%201/steps/build/debug-pod",
        protocols: ["scarab.v1", "scarab.csrf.tok-123"],
      },
    ]);
  });

  it("opens no socket when the check is refused, and says why", async () => {
    probeStatus = 403;
    const opened = await openShellSocket("r1", "build", "attach");
    expect(typeof opened).toBe("string");
    expect(sockets).toEqual([]);
  });
});
