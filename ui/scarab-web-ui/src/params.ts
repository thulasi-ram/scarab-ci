// Pure launch-parameter helpers (ADR-0043 §2, §6). No I/O, no framework — just
// the form model, static client validation, request mapping, server-error
// mapping, and re-run reconciliation. Kept pure so it reads as obviously
// correct (there is no JS test runner; correctness is carried by inspection +
// typecheck). The `validate:` CEL predicate is DELIBERATELY not evaluated here:
// static constraints only (required / type / options), CEL is server-only.

/** The closed parameter vocabulary — each maps to one bounded widget. */
export type ParamType = "string" | "boolean" | "number" | "choice";

/** A compiled launch-parameter spec, as served by the interface endpoint. The
 * generated schema types `inputs` as an opaque object (the pure crate can't
 * derive `ToSchema`), so this is the hand-authored mirror of
 * `scarab_pipeline::ParamSpec`. */
export type ParamSpec = {
  name: string;
  type: ParamType;
  required: boolean;
  default?: unknown;
  options?: string[];
  validate?: string;
  description?: string;
};

/** A single field's live value. Booleans are real bools (checkbox); everything
 * else is authored as a string in its widget and coerced at request time. */
export type FieldValue = string | boolean;

/** The initial value for one field: an optional param pre-fills its `default`
 * (ADR-0043 — optional ⇒ default is mandatory); a required param starts empty. */
export function paramInitialValue(spec: ParamSpec): FieldValue {
  if (spec.type === "boolean") {
    return spec.required ? false : Boolean(spec.default);
  }
  if (spec.required) return "";
  return spec.default === null || spec.default === undefined ? "" : String(spec.default);
}

/** Initial form state for a whole interface. */
export function initialValues(specs: ParamSpec[]): Record<string, FieldValue> {
  const out: Record<string, FieldValue> = {};
  for (const s of specs) out[s.name] = paramInitialValue(s);
  return out;
}

/** Static validation for one field — mirrors ONLY `required` / `type` /
 * `options` (ADR-0043 §6). Returns an error string, or null if statically OK.
 * Never evaluates `validate:` (server-only). */
export function validateField(spec: ParamSpec, value: FieldValue): string | null {
  if (spec.type === "boolean") return null; // a checkbox always has a value
  const s = typeof value === "string" ? value : String(value ?? "");
  const empty = s.trim() === "";
  if (empty) return spec.required ? "required" : null;
  if (spec.type === "number" && !Number.isFinite(Number(s))) {
    return "must be a number";
  }
  if (spec.type === "choice" && spec.options && !spec.options.includes(s)) {
    return "not one of the allowed options";
  }
  return null;
}

/** All field errors for the form (only fields that fail appear). Empty ⇒ the
 * form passes static validation and may be submitted. */
export function validateForm(
  specs: ParamSpec[],
  values: Record<string, FieldValue>,
): Record<string, string> {
  const errors: Record<string, string> = {};
  for (const s of specs) {
    const e = validateField(s, values[s.name] ?? paramInitialValue(s));
    if (e) errors[s.name] = e;
  }
  return errors;
}

/** Map the validated form to a dispatch `params` body (ADR-0043 §4): send
 * `number`/`boolean` as their JSON-typed values and `string`/`choice` as
 * strings. An empty non-boolean field is omitted so the server applies the
 * declared default (a required-empty field is blocked by validation first). */
export function toRequestParams(
  specs: ParamSpec[],
  values: Record<string, FieldValue>,
): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const spec of specs) {
    const v = values[spec.name];
    if (spec.type === "boolean") {
      out[spec.name] = Boolean(v);
      continue;
    }
    const s = typeof v === "string" ? v : String(v ?? "");
    if (s.trim() === "") continue;
    out[spec.name] = spec.type === "number" ? Number(s) : s;
  }
  return out;
}

/** The server's structured launch-parameter error, mapped to the form. The
 * dispatch 4xx body is plain text — an aggregate of `` `name`: message `` lines
 * (`resolve_params`). We attribute each line to its field; if nothing maps to a
 * known field, the whole message becomes a single form-level error. */
export function mapServerError(
  message: string,
  names: string[],
): { fieldErrors: Record<string, string>; formError: string | null } {
  const known = new Set(names);
  const fieldErrors: Record<string, string> = {};
  for (const rawLine of message.split("\n")) {
    const line = rawLine.replace(/^[\s-]+/, "").trim();
    if (!line) continue;
    // `name`: msg   or   `name` (default): msg
    const m = line.match(/^`([^`]+)`(?:\s*\([^)]*\))?\s*:\s*(.*)$/);
    if (m && known.has(m[1])) {
      fieldErrors[m[1]] = m[2] || line;
    }
  }
  const mapped = Object.keys(fieldErrors).length > 0;
  return { fieldErrors, formError: mapped ? null : message.trim() };
}

/** Reconcile a prior run's frozen params against the CURRENT interface for a
 * re-run (ADR-0043 §6): pre-fill values that still exist, drop prior params the
 * interface no longer declares (reported so the UI can note them). Type/validity
 * drift on a surviving param surfaces afterwards via `validateForm`. */
export function reconcilePrefill(
  specs: ParamSpec[],
  prior: Record<string, unknown>,
): { values: Record<string, FieldValue>; dropped: string[] } {
  const values: Record<string, FieldValue> = {};
  for (const spec of specs) {
    if (Object.prototype.hasOwnProperty.call(prior, spec.name)) {
      const raw = prior[spec.name];
      values[spec.name] =
        spec.type === "boolean"
          ? Boolean(raw)
          : raw === null || raw === undefined
            ? ""
            : String(raw);
    } else {
      values[spec.name] = paramInitialValue(spec);
    }
  }
  const declared = new Set(specs.map((s) => s.name));
  const dropped = Object.keys(prior).filter((k) => !declared.has(k));
  return { values, dropped };
}
