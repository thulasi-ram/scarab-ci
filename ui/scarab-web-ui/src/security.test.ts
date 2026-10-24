// Security shaping (ADR-0073): the app banner, the posture line, event rows,
// passkey badges, and the confirm page's words for each outcome.
import { describe, expect, it } from "vitest";
import {
  EVENT_KINDS,
  bannerText,
  callerLabel,
  canResetPasskeys,
  eventKindLabel,
  eventRow,
  passkeyBadges,
  passkeyLabel,
  postureText,
  requestConfirmable,
  requestErrorLink,
  requestErrorText,
  resetCandidates,
  resultSecret,
  resultText,
  shouldAutoEnroll,
  wantsEnroll,
} from "./security";

const summary = (enforced: boolean, without: string[], total = 3) => ({
  enforced,
  admins_without_passkey: without,
  admins_total: total,
});

describe("bannerText", () => {
  it("names how many admins hold no passkey while step-up is enforced", () => {
    expect(bannerText(summary(true, ["j.lee", "m.ortiz"]))).toBe(
      "step-up enforced; 2 admins have no passkey",
    );
    expect(bannerText(summary(true, ["j.lee"]))).toBe("step-up enforced; 1 admin has no passkey");
  });

  it("is hidden when there is nothing to act on", () => {
    // Off: nothing is refused. All enrolled: nobody is locked out.
    expect(bannerText(summary(false, ["j.lee"]))).toBeNull();
    expect(bannerText(summary(true, []))).toBeNull();
    // Not an admin anywhere (no summary fetched, or it was refused).
    expect(bannerText(null)).toBeNull();
    expect(bannerText(undefined)).toBeNull();
  });
});

describe("postureText", () => {
  it("counts the admins who CAN confirm, out of all of them", () => {
    expect(postureText(summary(true, ["j.lee"], 3))).toBe(
      "Step-up is enforced. 2 of 3 admins hold a passkey.",
    );
    expect(postureText(summary(true, [], 1))).toBe("Step-up is enforced. 1 of 1 admin holds a passkey.");
    expect(postureText(summary(false, [], 3))).toMatch(/off/);
  });
});

describe("event rows", () => {
  const base = { seq: 7, at: 1_700_000_000_000, subject: "a.kim", caller: "session:cookie" };

  it("a refusal row reads the op, target, mode and reason", () => {
    const row = eventRow({
      ...base,
      kind: "step_up.refused",
      op: "secret.put",
      target: "acme/web/prod/DB_PASSWORD",
      detail: { mode: "window", reason: "not confirmed" },
    });
    expect(row).toEqual({
      key: 7,
      at: base.at,
      kind: "step_up.refused",
      label: "Refused: step-up required",
      tone: "warn",
      who: "a.kim",
      via: "browser",
      what: "secret.put · acme/web/prod/DB_PASSWORD",
      note: "mode window · not confirmed",
    });
  });

  it("an event with neither op nor target shows a dash, and an empty detail no note", () => {
    const row = eventRow({ ...base, kind: "passkey.enrolled", detail: {} });
    expect(row.what).toBe("—");
    expect(row.note).toBe("");
    expect(row.tone).toBe("plain");
  });

  it("a clone-suspected passkey is the loud one", () => {
    expect(eventRow({ ...base, kind: "passkey.clone_suspected" }).tone).toBe("danger");
    expect(eventRow({ ...base, kind: "step_up.confirmed" }).tone).toBe("ok");
  });

  it("every kind the server writes has a label; an unknown one shows verbatim", () => {
    for (const k of EVENT_KINDS) expect(eventKindLabel(k)).not.toBe(k);
    expect(eventKindLabel("future.kind")).toBe("future.kind");
  });

  it("names how the caller came in", () => {
    expect(callerLabel("session:bearer")).toBe("CLI session");
    expect(callerLabel("api_token:tok-9")).toBe("API token tok-9");
    expect(callerLabel("operator:break-glass")).toBe("operator");
  });
});

describe("passkeys", () => {
  const pk = (o: Partial<Parameters<typeof passkeyBadges>[0]> = {}) => ({
    label: "MacBook",
    created_at: 1,
    last_used_at: null,
    backup_eligible: true,
    backup_state: true,
    clone_suspected: false,
    ...o,
  });

  it("says synced, syncable or device-bound", () => {
    expect(passkeyBadges(pk()).map((b) => b.text)).toEqual(["synced"]);
    expect(passkeyBadges(pk({ backup_state: false })).map((b) => b.text)).toEqual(["syncable"]);
    expect(passkeyBadges(pk({ backup_state: false, backup_eligible: false })).map((b) => b.text)).toEqual([
      "device-bound",
    ]);
  });

  it("flags a clone-suspected passkey first, as refused", () => {
    const b = passkeyBadges(pk({ clone_suspected: true }));
    expect(b[0]).toEqual({ text: "clone suspected — refused", tone: "danger" });
  });

  it("?enroll=1 starts enrollment; a blank label is left to the server's default", () => {
    expect(wantsEnroll("?enroll=1")).toBe(true);
    expect(wantsEnroll("?enroll=0")).toBe(false);
    expect(wantsEnroll("")).toBe(false);
  });

  it("auto-starts enrollment only for someone with no passkey yet", () => {
    const on = { step_up: "enforce", has_passkey: false };
    expect(shouldAutoEnroll("?enroll=1", on)).toBe(true);
    expect(shouldAutoEnroll("?enroll=1", { ...on, has_passkey: true })).toBe(false);
    expect(shouldAutoEnroll("?enroll=1", { ...on, step_up: "off" })).toBe(false);
    expect(shouldAutoEnroll("", on)).toBe(false);
    expect(shouldAutoEnroll("?enroll=1", null)).toBe(false);
  });

  it("leaves a blank passkey label to the server's default", () => {
    expect(passkeyLabel("   ")).toBeUndefined();
    expect(passkeyLabel(" YubiKey ")).toBe("YubiKey");
    expect(passkeyLabel("x".repeat(80))).toHaveLength(64);
  });
});

describe("Settings → Security", () => {
  it("only an Owner may reset passkeys", () => {
    expect(canResetPasskeys({ caller_role: "owner" })).toBe(true);
    expect(canResetPasskeys({ caller_role: "admin" })).toBe(false);
    expect(canResetPasskeys(null)).toBe(false);
  });

  it("lists each member once, marking the admins without a passkey", () => {
    expect(
      resetCandidates(
        [
          { subject: "j.lee", display_name: "Jordan Lee" },
          { subject: "a.kim", display_name: "Avery Kim" },
          { subject: "j.lee", display_name: "Jordan Lee" },
          { subject: "bot", display_name: null },
        ],
        ["j.lee"],
      ),
    ).toEqual([
      { subject: "a.kim", name: "Avery Kim", noPasskey: false },
      { subject: "bot", name: "bot", noPasskey: false },
      { subject: "j.lee", name: "Jordan Lee", noPasskey: true },
    ]);
  });
});

describe("the confirm page", () => {
  const NOW = 1_700_000_000_000;

  it("is confirmable only while pending and unexpired", () => {
    expect(requestConfirmable({ state: "pending", expires_at: NOW + 1 }, NOW)).toBe(true);
    expect(requestConfirmable({ state: "pending", expires_at: NOW }, NOW)).toBe(false);
    expect(requestConfirmable({ state: "executed", expires_at: NOW + 1 }, NOW)).toBe(false);
  });

  it("words 404, 409 and 410 for a human", () => {
    expect(requestErrorText(404, "not found")).toMatch(/No such request/);
    expect(requestErrorText(409, "the request is executed")).toMatch(/^Already confirmed/);
    expect(requestErrorText(410, "expired")).toMatch(/expired/);
    expect(requestErrorText(400, "challenge expired")).toBe("challenge expired");
  });

  it("reads a JSON error body as its message, never raw JSON", () => {
    const t = requestErrorText(409, '{"code":"x","message":"m"}');
    expect(t).toContain("m");
    expect(t).not.toContain('{"code"');
    expect(requestErrorText(400, '{"code":"challenge_invalid","message":"stale"}')).toBe("stale");
  });

  it("points a no-passkey refusal at enrollment", () => {
    expect(requestErrorLink(400, '{"code":"no_passkey","message":"enroll one first"}')).toEqual({
      text: "Enroll a passkey first",
      href: "/account/security?enroll=1",
    });
    expect(requestErrorLink(400, '{"code":"challenge_invalid"}')).toBeNull();
    expect(requestErrorLink(404, '{"code":"no_passkey"}')).toBeNull();
    expect(requestErrorLink(400, "plain")).toBeNull();
  });

  it("shows a minted token once, and never twice", () => {
    const body = { token: "scarab_pat_abc", record: { id: "t1", name: "ci" } };
    expect(resultSecret(body)).toBe("scarab_pat_abc");
    // The JSON beside it leaves the secret out.
    expect(resultText(body)).not.toContain("scarab_pat_abc");
    expect(JSON.parse(resultText(body))).toEqual({ record: { id: "t1", name: "ci" } });
  });

  it("shows any other result as data", () => {
    expect(resultSecret({ removed: 2 })).toBeNull();
    expect(resultText({ removed: 2 })).toBe('{\n  "removed": 2\n}');
    expect(resultText("deleted")).toBe("deleted");
    expect(resultText(null)).toBe("");
  });
});
