// WebAuthn in the browser (ADR-0073): the JSON ↔ ArrayBuffer seam between what
// the server sends (`PublicKeyCredential*OptionsJSON`, every byte string as
// base64url) and what `navigator.credentials.*` takes and returns (raw
// ArrayBuffers), and back again (`RegistrationResponseJSON` /
// `AuthenticationResponseJSON`, which is what the server parses).
//
// Browsers that implement WebAuthn L3 do the conversion natively —
// `PublicKeyCredential.parseCreationOptionsFromJSON`,
// `parseRequestOptionsFromJSON` and `credential.toJSON()` — and those are used
// when present. Older ones (Safari before 18, Firefox before 119) get the manual
// fallbacks below, which convert exactly the fields the server sends and
// nothing else. The fallbacks are plain functions over plain objects, so the
// test suite exercises them with no DOM.
//
// base64url here is RFC 4648 §5 WITHOUT padding — what `toJSON()` produces and
// what the server's decoder (URL_SAFE_NO_PAD) expects.

/** Encode bytes as unpadded base64url. */
export function toB64url(data: ArrayBuffer | ArrayBufferView): string {
  const bytes =
    data instanceof ArrayBuffer
      ? new Uint8Array(data)
      : new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  let bin = "";
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]!);
  return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/** Decode base64url (padded or not; plain base64's `+/` tolerated too). */
export function fromB64url(s: string): ArrayBuffer {
  const b64 = s.replace(/-/g, "+").replace(/_/g, "/").replace(/=+$/, "");
  const padded = b64 + "=".repeat((4 - (b64.length % 4)) % 4);
  const bin = atob(padded);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out.buffer;
}

// --- options: server JSON → navigator.credentials.* arguments -------------

type DescriptorJSON = { type: string; id: string; transports?: string[] };

/** The options JSON shapes the server sends, structurally — only the
 * byte-string fields matter to the conversion; the rest pass through. */
export type CreationOptionsJSON = {
  challenge: string;
  user: { id: string; name: string; displayName: string };
  excludeCredentials?: DescriptorJSON[];
  [k: string]: unknown;
};
export type RequestOptionsJSON = {
  challenge: string;
  allowCredentials?: DescriptorJSON[];
  [k: string]: unknown;
};

/** The static parsers a WebAuthn L3 browser has; absent on older ones. */
export type NativeParsers = {
  parseCreationOptionsFromJSON?: (o: never) => PublicKeyCredentialCreationOptions;
  parseRequestOptionsFromJSON?: (o: never) => PublicKeyCredentialRequestOptions;
};

/** `window.PublicKeyCredential`, or `undefined` outside a WebAuthn browser. */
function nativeParsers(): NativeParsers | undefined {
  return typeof PublicKeyCredential === "undefined"
    ? undefined
    : (PublicKeyCredential as unknown as NativeParsers);
}

const descriptor = (d: DescriptorJSON): PublicKeyCredentialDescriptor => ({
  ...(d as unknown as PublicKeyCredentialDescriptor),
  type: "public-key",
  id: fromB64url(d.id),
});

/** `navigator.credentials.create({ publicKey })`'s argument from the server's
 * `PublicKeyCredentialCreationOptionsJSON`. */
export function creationOptionsFromJSON(
  json: CreationOptionsJSON,
  native: NativeParsers | undefined = nativeParsers(),
): PublicKeyCredentialCreationOptions {
  if (typeof native?.parseCreationOptionsFromJSON === "function") {
    return native.parseCreationOptionsFromJSON(json as never);
  }
  return {
    ...(json as unknown as PublicKeyCredentialCreationOptions),
    challenge: fromB64url(json.challenge),
    user: { ...json.user, id: fromB64url(json.user.id) },
    excludeCredentials: (json.excludeCredentials ?? []).map(descriptor),
  };
}

/** `navigator.credentials.get({ publicKey })`'s argument from the server's
 * `PublicKeyCredentialRequestOptionsJSON`. */
export function requestOptionsFromJSON(
  json: RequestOptionsJSON,
  native: NativeParsers | undefined = nativeParsers(),
): PublicKeyCredentialRequestOptions {
  if (typeof native?.parseRequestOptionsFromJSON === "function") {
    return native.parseRequestOptionsFromJSON(json as never);
  }
  return {
    ...(json as unknown as PublicKeyCredentialRequestOptions),
    challenge: fromB64url(json.challenge),
    allowCredentials: (json.allowCredentials ?? []).map(descriptor),
  };
}

// --- credential → server JSON ---------------------------------------------

/** What `navigator.credentials.*` resolves to, structurally: the parts the
 * conversion reads. A real `PublicKeyCredential` satisfies it. */
export type CredentialLike = {
  id: string;
  rawId: ArrayBuffer;
  type: string;
  authenticatorAttachment?: string | null;
  response: {
    clientDataJSON: ArrayBuffer;
    // registration
    attestationObject?: ArrayBuffer;
    getTransports?: () => string[];
    // assertion
    authenticatorData?: ArrayBuffer;
    signature?: ArrayBuffer;
    userHandle?: ArrayBuffer | null;
  };
  getClientExtensionResults?: () => unknown;
  toJSON?: () => unknown;
};

/** `RegistrationResponseJSON` / `AuthenticationResponseJSON` for the server —
 * the native `toJSON()` when the browser has it, else built by hand. */
export function credentialToJSON(cred: CredentialLike): Record<string, unknown> {
  if (typeof cred.toJSON === "function") {
    return cred.toJSON() as Record<string, unknown>;
  }
  const r = cred.response;
  const response: Record<string, unknown> = { clientDataJSON: toB64url(r.clientDataJSON) };
  if (r.attestationObject) {
    response.attestationObject = toB64url(r.attestationObject);
    response.transports = typeof r.getTransports === "function" ? r.getTransports() : [];
  } else {
    response.authenticatorData = toB64url(r.authenticatorData ?? new ArrayBuffer(0));
    response.signature = toB64url(r.signature ?? new ArrayBuffer(0));
    if (r.userHandle) response.userHandle = toB64url(r.userHandle);
  }
  return {
    id: cred.id,
    rawId: toB64url(cred.rawId),
    type: cred.type,
    authenticatorAttachment: cred.authenticatorAttachment ?? undefined,
    clientExtensionResults:
      typeof cred.getClientExtensionResults === "function" ? cred.getClientExtensionResults() : {},
    response,
  };
}

// --- the two ceremonies ---------------------------------------------------

/** Is WebAuthn available in this browser at all? (An insecure origin — plain
 * http on anything but localhost — has no `navigator.credentials.create`.) */
export function webauthnAvailable(): boolean {
  return (
    typeof window !== "undefined" &&
    typeof PublicKeyCredential !== "undefined" &&
    typeof navigator?.credentials?.get === "function"
  );
}

/** Make a passkey from the server's creation options; the registration
 * response JSON to POST back. */
export async function createPasskey(publicKey: unknown): Promise<Record<string, unknown>> {
  const cred = await navigator.credentials.create({
    publicKey: creationOptionsFromJSON(publicKey as CreationOptionsJSON),
  });
  if (!cred) throw new Error("The browser returned no passkey.");
  return credentialToJSON(cred as unknown as CredentialLike);
}

/** Sign the server's challenge with a passkey; the assertion JSON to POST
 * back. */
export async function getAssertion(publicKey: unknown): Promise<Record<string, unknown>> {
  const cred = await navigator.credentials.get({
    publicKey: requestOptionsFromJSON(publicKey as RequestOptionsJSON),
  });
  if (!cred) throw new Error("The browser returned no passkey assertion.");
  return credentialToJSON(cred as unknown as CredentialLike);
}

/** A WebAuthn failure in words. `NotAllowedError` is what every browser throws
 * for "the person dismissed the prompt" AND for "timed out" — indistinguishable
 * by design — so it is reported as the benign one. */
export function webauthnErrorText(e: unknown): string {
  const name = (e as { name?: string } | null)?.name;
  if (name === "NotAllowedError") return "The passkey prompt was dismissed or timed out.";
  if (name === "InvalidStateError") return "This authenticator already holds a passkey for you.";
  if (name === "SecurityError")
    return "This page's address does not match the server's passkey domain.";
  return e instanceof Error ? e.message : "The passkey ceremony failed.";
}
