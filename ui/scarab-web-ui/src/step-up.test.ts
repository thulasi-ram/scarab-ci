// Step-up in the UI (ADR-0073): reading the 403, and the confirm-and-retry
// loop every crown-jewel call in the app goes through. The ceremony (dialog +
// passkey) is the one true external here and is a plain function; the
// responses are real `Response`s.
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  MAX_STEP_UP_ROUNDS,
  askStepUp,
  errorMessage,
  parseStepUp,
  shareCeremony,
  stepUpPrompt,
  withStepUp,
  type Ceremony,
  type StepUpRequired,
} from "./step-up";

const summary = { operation: "Write a secret", target: "acme/web/prod/DB_PASSWORD", changes: ["value = (hidden)"] };
const challenge = { id: "ch-1", public_key: { challenge: "AAAA", rpId: "ci.example.com" }, expires_at: 1 };

const refusalBody = (mode: string, extra: Record<string, unknown> = {}) => ({
  code: "step_up_required",
  message: "This needs a step-up confirmation: confirm with your passkey.",
  mode,
  op: "secret.put",
  summary,
  ...(mode === "enroll" ? { enroll_url: "/account/security?enroll=1" } : { challenge }),
  ...extra,
});

const res403 = (body: unknown) =>
  new Response(typeof body === "string" ? body : JSON.stringify(body), {
    status: 403,
    headers: { "content-type": "application/json" },
  });
const ok = () => new Response(JSON.stringify({ ok: true }), { status: 200 });

describe("parseStepUp", () => {
  it("reads each mode the server sends", () => {
    for (const mode of ["window", "action", "enroll"]) {
      const r = parseStepUp(refusalBody(mode));
      expect(r?.code).toBe("step_up_required");
      expect((r as StepUpRequired).mode).toBe(mode);
      expect((r as StepUpRequired).summary).toEqual(summary);
    }
    expect((parseStepUp(refusalBody("window")) as StepUpRequired).challenge?.id).toBe("ch-1");
    expect((parseStepUp(refusalBody("enroll")) as StepUpRequired).enroll_url).toBe(
      "/account/security?enroll=1",
    );
  });

  it("reads a fresh_login_required with its login_url", () => {
    expect(
      parseStepUp({ code: "fresh_login_required", message: "Log in again", login_url: "/v1/auth/login?next=x" }),
    ).toEqual({ code: "fresh_login_required", message: "Log in again", login_url: "/v1/auth/login?next=x" });
  });

  it("is null for every other 403 — never guesses a dialog", () => {
    expect(parseStepUp("insufficient role")).toBeNull();
    expect(parseStepUp({ code: "clone_suspected", message: "x" })).toBeNull();
    // An unknown (future) mode is passed through as the plain 403 it is.
    expect(parseStepUp(refusalBody("telepathy"))).toBeNull();
    // A refusal with no summary cannot show what is being confirmed.
    expect(parseStepUp({ ...refusalBody("window"), summary: undefined })).toBeNull();
    expect(parseStepUp(null)).toBeNull();
  });

  it("accepts the raw JSON text too", () => {
    expect(parseStepUp(JSON.stringify(refusalBody("action")))?.code).toBe("step_up_required");
  });
});

describe("withStepUp", () => {
  const counting = (answers: boolean[]) => {
    const seen: StepUpRequired[] = [];
    const ceremony: Ceremony = async (r) => {
      seen.push(r);
      return answers.shift() ?? false;
    };
    return { ceremony, seen };
  };

  it("confirms once and retries the request once", async () => {
    const { ceremony, seen } = counting([true]);
    const retry = vi.fn(async () => ok());
    const res = await withStepUp(res403(refusalBody("window")), retry, ceremony);
    expect(res.status).toBe(200);
    expect(retry).toHaveBeenCalledTimes(1);
    expect(seen.map((r) => r.mode)).toEqual(["window"]);
  });

  it("a cancel returns the original 403, body unread", async () => {
    const { ceremony } = counting([false]);
    const retry = vi.fn(async () => ok());
    const first = res403(refusalBody("action"));
    const res = await withStepUp(first, retry, ceremony);
    expect(res).toBe(first);
    expect(retry).not.toHaveBeenCalled();
    // The caller still reads the server's own words.
    expect(((await res.json()) as { code: string }).code).toBe("step_up_required");
  });

  it("enroll mode asks (the dialog links to enrollment) and, unanswered, returns the 403", async () => {
    const { ceremony, seen } = counting([false]);
    const retry = vi.fn(async () => ok());
    const res = await withStepUp(res403(refusalBody("enroll")), retry, ceremony);
    expect(res.status).toBe(403);
    expect(seen[0]!.mode).toBe("enroll");
    expect(seen[0]!.enroll_url).toBe("/account/security?enroll=1");
    expect(retry).not.toHaveBeenCalled();
  });

  it("chains a window then an action: two ceremonies, two retries", async () => {
    const { ceremony, seen } = counting([true, true]);
    const answers = [res403(refusalBody("action")), ok()];
    const retry = vi.fn(async () => answers.shift()!);
    const res = await withStepUp(res403(refusalBody("window")), retry, ceremony);
    expect(res.status).toBe(200);
    expect(seen.map((r) => r.mode)).toEqual(["window", "action"]);
    expect(retry).toHaveBeenCalledTimes(2);
  });

  it(`stops after ${MAX_STEP_UP_ROUNDS} rounds rather than trapping the user in dialogs`, async () => {
    const { ceremony, seen } = counting([true, true, true, true]);
    const retry = vi.fn(async () => res403(refusalBody("action")));
    const res = await withStepUp(res403(refusalBody("window")), retry, ceremony);
    expect(res.status).toBe(403);
    expect(seen).toHaveLength(MAX_STEP_UP_ROUNDS);
    expect(retry).toHaveBeenCalledTimes(MAX_STEP_UP_ROUNDS);
  });

  it("passes any other 403 straight through", async () => {
    const ceremony = vi.fn(async () => true);
    const first = res403("insufficient role");
    expect(await withStepUp(first, async () => ok(), ceremony)).toBe(first);
    expect(ceremony).not.toHaveBeenCalled();
  });
});

describe("shareCeremony", () => {
  it("concurrent refusals share ONE in-flight ceremony", async () => {
    let release!: (v: boolean) => void;
    const run = vi.fn(() => new Promise<boolean>((r) => (release = r)));
    const shared = vi.fn(shareCeremony(run));
    const retries = [vi.fn(async () => ok()), vi.fn(async () => ok()), vi.fn(async () => ok())];
    const all = Promise.all(
      retries.map((retry) => withStepUp(res403(refusalBody("window")), retry, shared)),
    );
    // All three are waiting on the dialog before the human answers it.
    await vi.waitFor(() => expect(shared).toHaveBeenCalledTimes(3));
    expect(run).toHaveBeenCalledTimes(1);
    release(true);
    const out = await all;
    expect(out.map((r) => r.status)).toEqual([200, 200, 200]);
    expect(run).toHaveBeenCalledTimes(1);
    for (const r of retries) expect(r).toHaveBeenCalledTimes(1);
  });

  it("a cancel answers every sharer with its own 403, and the next refusal asks again", async () => {
    const run = vi.fn(async () => false);
    const shared = shareCeremony(run);
    const firsts = [res403(refusalBody("window")), res403(refusalBody("window"))];
    const out = await Promise.all(firsts.map((f) => withStepUp(f, async () => ok(), shared)));
    expect(out[0]).toBe(firsts[0]);
    expect(out[1]).toBe(firsts[1]);
    const before = run.mock.calls.length;
    await shared(parseStepUp(refusalBody("window")) as StepUpRequired);
    expect(run.mock.calls.length).toBe(before + 1);
  });
});

describe("the dialog's state", () => {
  afterEach(() => stepUpPrompt()?.settle(false));

  it("askStepUp opens the prompt and resolves with the answer, closing it", async () => {
    const refusal = parseStepUp(refusalBody("window")) as StepUpRequired;
    const answer = askStepUp(refusal);
    expect(stepUpPrompt()?.refusal).toBe(refusal);
    stepUpPrompt()!.settle(true);
    expect(await answer).toBe(true);
    expect(stepUpPrompt()).toBeNull();
  });
});

describe("errorMessage", () => {
  it("reads a JSON error's message, else the text", () => {
    expect(errorMessage('{"code":"no_passkey","message":"enroll one first"}', "x")).toBe("enroll one first");
    expect(errorMessage("insufficient role", "x")).toBe("insufficient role");
    expect(errorMessage("  ", "fallback")).toBe("fallback");
  });
});

// The wiring: the API client's middleware and its plain-fetch `send` path,
// against a stubbed `fetch` (the server is the external). openapi-fetch binds
// `fetch` when the client module is first evaluated, so each test imports a
// fresh copy after stubbing.
describe("the API client retries a refused request after the confirmation", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.resetModules();
  });

  const fresh = async (responses: Response[]) => {
    const calls: Request[] = [];
    // Node's Request wants an absolute URL; the browser resolves the client's
    // relative ones against the page.
    const RealRequest = globalThis.Request;
    const abs = (u: string) => (u.startsWith("/") ? `http://scarab.test${u}` : u);
    class PageRequest extends RealRequest {
      constructor(input: RequestInfo | URL, init?: RequestInit) {
        super(typeof input === "string" ? abs(input) : input, init);
      }
    }
    vi.stubGlobal("Request", PageRequest);
    vi.stubGlobal("document", { cookie: "scarab_csrf=tok-123" });
    vi.stubGlobal("fetch", async (input: RequestInfo | URL, init?: RequestInit) => {
      calls.push(
        input instanceof RealRequest
          ? new RealRequest(input, init)
          : new RealRequest(abs(String(input)), init),
      );
      return responses.shift() ?? ok();
    });
    vi.resetModules();
    const client = await import("./api/client");
    const stepUp = await import("./step-up");
    return { client, stepUp, calls };
  };

  it("through the `api` middleware: same request again, CSRF intact", async () => {
    const { client, stepUp, calls } = await fresh([
      res403(refusalBody("action")),
      new Response(null, { status: 204 }),
    ]);
    const done = client.removePasskey("pk-1");
    await vi.waitFor(() => expect(stepUp.stepUpPrompt()).not.toBeNull());
    expect(stepUp.stepUpPrompt()!.refusal.summary.operation).toBe("Write a secret");
    stepUp.stepUpPrompt()!.settle(true);
    await done;
    expect(calls.map((c) => `${c.method} ${new URL(c.url).pathname}`)).toEqual([
      "DELETE /v1/me/passkeys/pk-1",
      "DELETE /v1/me/passkeys/pk-1",
    ]);
    expect(calls[1]!.headers.get("x-csrf-token")).toBe("tok-123");
  });

  it("through the `api` middleware: a typed POST is retried with its body", async () => {
    const { client, stepUp, calls } = await fresh([
      res403(refusalBody("action")),
      new Response(JSON.stringify({ id: "c1", public_key: {}, expires_at: 1 }), {
        status: 200,
        headers: { "content-type": "application/json" },
      }),
    ]);
    const done = client.beginPasskeyRegistration("x");
    await vi.waitFor(() => expect(stepUp.stepUpPrompt()).not.toBeNull());
    stepUp.stepUpPrompt()!.settle(true);
    await done;
    expect(calls).toHaveLength(2);
    const first = await calls[0]!.text();
    expect(first).toBe(JSON.stringify({ label: "x" }));
    expect(await calls[1]!.text()).toBe(first);
    expect(calls[1]!.headers.get("x-csrf-token")).toBe("tok-123");
  });

  it("through the `api` middleware: a cancel surfaces the server's message", async () => {
    const { client, stepUp } = await fresh([res403(refusalBody("action"))]);
    const done = client.removePasskey("pk-1");
    await vi.waitFor(() => expect(stepUp.stepUpPrompt()).not.toBeNull());
    stepUp.stepUpPrompt()!.settle(false);
    await expect(done).rejects.toThrow("This needs a step-up confirmation");
  });

  it("through the plain-fetch path (role bindings): same body again", async () => {
    const { client, stepUp, calls } = await fresh([
      res403(refusalBody("window")),
      new Response(null, { status: 204 }),
    ]);
    const done = client.grantRole("acme", { subject: "j.lee", role: "admin" });
    await vi.waitFor(() => expect(stepUp.stepUpPrompt()).not.toBeNull());
    stepUp.stepUpPrompt()!.settle(true);
    await done;
    expect(calls).toHaveLength(2);
    expect(await calls[1]!.text()).toBe(JSON.stringify({ subject: "j.lee", role: "admin" }));
    expect(calls[1]!.headers.get("x-csrf-token")).toBe("tok-123");
  });
});
