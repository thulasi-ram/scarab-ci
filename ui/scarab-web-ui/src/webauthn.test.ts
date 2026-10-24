// The WebAuthn JSON ↔ bytes seam (ADR-0073). The manual fallbacks are what a
// browser without the L3 JSON helpers (Safari < 18, Firefox < 119) runs, and
// a byte off here is a passkey the server refuses with "challenge mismatch" —
// so they are checked against fixed vectors, not only against themselves.
import { describe, expect, it } from "vitest";
import {
  creationOptionsFromJSON,
  credentialToJSON,
  fromB64url,
  requestOptionsFromJSON,
  toB64url,
  webauthnErrorText,
  type CredentialLike,
} from "./webauthn";

const bytes = (...b: number[]) => new Uint8Array(b);
const asBytes = (buf: ArrayBuffer | BufferSource) =>
  Array.from(buf instanceof ArrayBuffer ? new Uint8Array(buf) : new Uint8Array((buf as ArrayBufferView).buffer));

describe("base64url", () => {
  it("encodes unpadded, URL-safe (the alphabet's two differing characters included)", () => {
    expect(toB64url(bytes(0xfb, 0xff))).toBe("-_8");
    expect(toB64url(bytes())).toBe("");
    expect(toB64url(bytes(0x66))).toBe("Zg");
    expect(toB64url(new TextEncoder().encode("hello"))).toBe("aGVsbG8");
  });

  it("decodes padded and unpadded alike", () => {
    expect(asBytes(fromB64url("-_8"))).toEqual([0xfb, 0xff]);
    expect(asBytes(fromB64url("Zg=="))).toEqual([0x66]);
    expect(asBytes(fromB64url("Zg"))).toEqual([0x66]);
  });

  it("round-trips every length and every byte value", () => {
    const all = Array.from({ length: 256 }, (_, i) => i);
    for (let n = 0; n <= 70; n++) {
      const b = new Uint8Array(all.slice(0, n).map((x) => (x * 37 + n) & 0xff));
      expect(asBytes(fromB64url(toB64url(b)))).toEqual(Array.from(b));
    }
    expect(asBytes(fromB64url(toB64url(new Uint8Array(all))))).toEqual(all);
  });

  it("encodes a view of part of a buffer, not the whole buffer", () => {
    const buf = bytes(1, 2, 3, 4, 5);
    expect(toB64url(buf.subarray(1, 3))).toBe(toB64url(bytes(2, 3)));
  });
});

describe("options from the server's JSON (manual fallback)", () => {
  const challenge = toB64url(bytes(9, 8, 7, 6));
  const credId = toB64url(bytes(0xaa, 0xbb));

  it("creation options: challenge, user.id and excluded ids become bytes; the rest passes through", () => {
    const o = creationOptionsFromJSON(
      {
        rp: { id: "ci.example.com", name: "Scarab" },
        user: { id: toB64url(bytes(1, 2)), name: "a.kim", displayName: "Avery Kim" },
        challenge,
        pubKeyCredParams: [{ type: "public-key", alg: -7 }],
        excludeCredentials: [{ type: "public-key", id: credId, transports: ["internal"] }],
        authenticatorSelection: { userVerification: "required" },
      },
      undefined,
    );
    expect(asBytes(o.challenge)).toEqual([9, 8, 7, 6]);
    expect(asBytes(o.user.id)).toEqual([1, 2]);
    expect(o.user.name).toBe("a.kim");
    expect(asBytes(o.excludeCredentials![0]!.id)).toEqual([0xaa, 0xbb]);
    expect(o.excludeCredentials![0]!.transports).toEqual(["internal"]);
    expect(o.rp.id).toBe("ci.example.com");
    expect(o.authenticatorSelection?.userVerification).toBe("required");
  });

  it("request options: challenge and allowed ids become bytes", () => {
    const o = requestOptionsFromJSON(
      { challenge, rpId: "ci.example.com", allowCredentials: [{ type: "public-key", id: credId }], userVerification: "required" },
      undefined,
    );
    expect(asBytes(o.challenge)).toEqual([9, 8, 7, 6]);
    expect(asBytes(o.allowCredentials![0]!.id)).toEqual([0xaa, 0xbb]);
    expect(o.rpId).toBe("ci.example.com");
  });

  it("prefers the browser's own parser when it has one", () => {
    const native = {
      parseRequestOptionsFromJSON: () => ({ challenge: new ArrayBuffer(1), rpId: "native" }),
    };
    expect(requestOptionsFromJSON({ challenge }, native).rpId).toBe("native");
  });
});

describe("credentials to the server's JSON (manual fallback)", () => {
  const buf = (...b: number[]) => bytes(...b).buffer;

  it("an assertion carries authenticatorData, signature and userHandle as base64url", () => {
    const cred: CredentialLike = {
      id: toB64url(bytes(0xaa, 0xbb)),
      rawId: buf(0xaa, 0xbb),
      type: "public-key",
      authenticatorAttachment: "platform",
      response: {
        clientDataJSON: buf(1),
        authenticatorData: buf(2, 2),
        signature: buf(3, 3, 3),
        userHandle: buf(4),
      },
      getClientExtensionResults: () => ({}),
    };
    expect(credentialToJSON(cred)).toEqual({
      id: "qrs",
      rawId: "qrs",
      type: "public-key",
      authenticatorAttachment: "platform",
      clientExtensionResults: {},
      response: {
        clientDataJSON: toB64url(bytes(1)),
        authenticatorData: toB64url(bytes(2, 2)),
        signature: toB64url(bytes(3, 3, 3)),
        userHandle: toB64url(bytes(4)),
      },
    });
  });

  it("a registration carries attestationObject and transports", () => {
    const json = credentialToJSON({
      id: "qrs",
      rawId: buf(0xaa, 0xbb),
      type: "public-key",
      response: {
        clientDataJSON: buf(1),
        attestationObject: buf(5, 6),
        getTransports: () => ["internal", "hybrid"],
      },
    });
    expect(json.response).toEqual({
      clientDataJSON: toB64url(bytes(1)),
      attestationObject: toB64url(bytes(5, 6)),
      transports: ["internal", "hybrid"],
    });
  });

  it("round-trips: what the server sent is what the server gets back", () => {
    const raw = bytes(0xde, 0xad, 0xbe, 0xef, 0x00, 0xff);
    const json = credentialToJSON({
      id: toB64url(raw),
      rawId: raw.buffer,
      type: "public-key",
      response: { clientDataJSON: raw.buffer, authenticatorData: raw.buffer, signature: raw.buffer },
    });
    expect(asBytes(fromB64url(json.rawId as string))).toEqual(Array.from(raw));
  });

  it("uses the browser's toJSON() when present", () => {
    const json = credentialToJSON({
      id: "x",
      rawId: buf(1),
      type: "public-key",
      response: { clientDataJSON: buf(1) },
      toJSON: () => ({ native: true }),
    });
    expect(json).toEqual({ native: true });
  });
});

describe("webauthnErrorText", () => {
  it("reads a dismissed prompt as dismissed, not as a failure", () => {
    expect(webauthnErrorText({ name: "NotAllowedError" })).toMatch(/dismissed/);
    expect(webauthnErrorText(new Error("challenge expired"))).toBe("challenge expired");
  });
});
