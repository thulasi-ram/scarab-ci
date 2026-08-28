import { describe, it, expect } from "vitest";
import {
  daysLeft,
  expiresSoon,
  expiryLabel,
  isLive,
  lifetimeError,
  MAX_TOKEN_DAYS,
  scopeLabel,
  sortTokens,
  tokenState,
} from "./api-tokens";

const NOW = 1_700_000_000_000;
const DAY = 86_400_000;

const token = (o: Partial<Parameters<typeof tokenState>[0]> = {}) => ({
  created_at: NOW - 30 * DAY,
  expires_at: NOW + 30 * DAY,
  revoked_at: null,
  ...o,
});

describe("tokenState", () => {
  it("is live while the clock has not arrived and nobody has revoked it", () => {
    expect(tokenState(token(), NOW)).toBe("live");
    expect(isLive(token(), NOW)).toBe(true);
  });

  it("expires exactly AT the stamp, not after it", () => {
    // The server's check is `expires_at <= now`; a token that authenticated at
    // its own expiry millisecond in the UI and not on the server would be the
    // worst kind of disagreement — the list says usable, the API says 401.
    expect(tokenState(token({ expires_at: NOW }), NOW)).toBe("expired");
    expect(tokenState(token({ expires_at: NOW + 1 }), NOW)).toBe("live");
  });

  it("reports revocation even when the token has since expired too", () => {
    // Both facts are true; only one of them is the moment the credential died,
    // and it is the one a human caused. Falling through to "expired" here would
    // quietly rewrite that.
    const t = token({ revoked_at: NOW - 40 * DAY, expires_at: NOW - 10 * DAY });
    expect(tokenState(t, NOW)).toBe("revoked");
    expect(isLive(t, NOW)).toBe(false);
  });
});

describe("daysLeft / expiryLabel", () => {
  it("rounds remaining life DOWN", () => {
    // 1.9 days left is "1 day", never "2": nobody should have to discover that
    // the number they read was rounded up in their favour.
    expect(daysLeft(token({ expires_at: NOW + DAY * 1.9 }), NOW)).toBe(1);
    expect(expiryLabel(token({ expires_at: NOW + DAY * 1.9 }), NOW)).toBe("expires in 1 day");
  });

  it("says `today` for the last day rather than `in 0 days`", () => {
    expect(expiryLabel(token({ expires_at: NOW + 3600_000 }), NOW)).toBe("expires today");
  });

  it("looks backwards once past", () => {
    expect(expiryLabel(token({ expires_at: NOW - 2 * DAY }), NOW)).toBe("expired 2 days ago");
    expect(expiryLabel(token({ expires_at: NOW - DAY }), NOW)).toBe("expired 1 day ago");
  });
});

describe("expiresSoon", () => {
  it("warns only inside the window, and only for tokens still worth renewing", () => {
    expect(expiresSoon(token({ expires_at: NOW + 13 * DAY }), NOW)).toBe(true);
    expect(expiresSoon(token({ expires_at: NOW + 40 * DAY }), NOW)).toBe(false);
  });

  it("never warns about a token that is already dead", () => {
    // "expires in 3 days" on a revoked credential is noise pointing at a
    // renewal nobody needs to do.
    expect(expiresSoon(token({ expires_at: NOW - DAY }), NOW)).toBe(false);
    expect(expiresSoon(token({ revoked_at: NOW - DAY, expires_at: NOW + 3 * DAY }), NOW)).toBe(
      false,
    );
  });
});

describe("sortTokens", () => {
  it("puts every usable token above every dead one", () => {
    const old = token({ created_at: NOW - 90 * DAY, expires_at: NOW + DAY });
    const revoked = token({ created_at: NOW - DAY, revoked_at: NOW - 60_000 });
    const expired = token({ created_at: NOW - 2 * DAY, expires_at: NOW - DAY });
    const order = sortTokens([revoked, expired, old], NOW);
    expect(order[0]).toBe(old);
    expect(order.slice(1)).toContain(revoked);
    expect(order.slice(1)).toContain(expired);
  });

  it("shows the newest live token first, so a fresh mint lands at the top", () => {
    const older = token({ created_at: NOW - 10 * DAY });
    const fresh = token({ created_at: NOW });
    expect(sortTokens([older, fresh], NOW)[0]).toBe(fresh);
  });

  it("orders dead tokens by when they died, not when they were made", () => {
    // An ancient token revoked this morning is the one an operator is looking
    // for; ordering the graveyard by mint date buries it.
    const diedToday = token({ created_at: NOW - 300 * DAY, revoked_at: NOW - 60_000 });
    const diedLastYear = token({ created_at: NOW - 10 * DAY, expires_at: NOW - 200 * DAY });
    expect(sortTokens([diedLastYear, diedToday], NOW)[0]).toBe(diedToday);
  });

  it("does not mutate its input", () => {
    const list = [token({ created_at: NOW - DAY }), token({ created_at: NOW })];
    const first = list[0];
    sortTokens(list, NOW);
    expect(list[0]).toBe(first);
  });
});

describe("lifetimeError", () => {
  it("refuses the shape this repo already learned not to mint twice", () => {
    // No zero, no negative, no "forever" — the server makes expiry required for
    // exactly this reason, and the form should say so before the round trip.
    expect(lifetimeError(0)).toBeTruthy();
    expect(lifetimeError(-1)).toBeTruthy();
  });

  it("accepts the server's whole range and nothing past it", () => {
    expect(lifetimeError(1)).toBeNull();
    expect(lifetimeError(MAX_TOKEN_DAYS)).toBeNull();
    expect(lifetimeError(MAX_TOKEN_DAYS + 1)).toBeTruthy();
  });

  it("rejects a fractional day", () => {
    // `expires_in_days` is an int32 on the wire; 1.5 would serialize and be
    // rejected server-side with a less specific message.
    expect(lifetimeError(1.5)).toBeTruthy();
    expect(lifetimeError(Number.NaN)).toBeTruthy();
  });
});

describe("scopeLabel", () => {
  it("names the org alone when the token covers all of it", () => {
    expect(scopeLabel({ org: "acme", project: null })).toBe("acme");
    expect(scopeLabel({ org: "acme" })).toBe("acme");
  });

  it("names the project when the token is narrowed to one", () => {
    expect(scopeLabel({ org: "acme", project: "orders-api" })).toBe("acme/orders-api");
  });
});
