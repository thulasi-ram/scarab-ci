// The shared form controls: a labelled Field and the controls that go in it
// (Input, Select, Textarea, Checkbox). Every control is a 30px hairline box on
// the type scale; focus draws an emerald border and ring (DESIGN.md §6 Forms).
//
// A Field owns the label, the required marker, the help line and the error
// line. The control inside it picks up the Field's id and its
// `aria-describedby` / `aria-invalid` from context, so a caller never wires
// ids by hand:
//
//   <Field label="Allowed refs" help="Full refs, comma-separated.">
//     <Input mono value={refs()} onValue={setRefs} />
//   </Field>
//
// Mono is for machine values only (refs, URLs, ids, hashes): pass `mono`.
// Names and prose stay in the UI face.
import {
  createContext,
  createUniqueId,
  splitProps,
  Show,
  useContext,
  type JSX,
} from "solid-js";
import Icon from "./Icon";

type FieldCtx = { id: string; describedBy: () => string | undefined; invalid: () => boolean };
const FieldContext = createContext<FieldCtx>();

export function Field(props: {
  label: JSX.Element;
  /** The help line under the control. Hidden while `error` is set. */
  help?: JSX.Element;
  /** A problem with the value; replaces the help line and marks the control. */
  error?: string | null;
  required?: boolean;
  /** Set when the control needs a known id (a test, an external label). */
  id?: string;
  /** Show the label in Body, not Label (a key name, a typed sentence). */
  plainLabel?: boolean;
  class?: string;
  children: JSX.Element;
}) {
  const id = props.id ?? createUniqueId();
  const helpId = `${id}-help`;
  const errId = `${id}-err`;
  const ctx: FieldCtx = {
    id,
    describedBy: () => (props.error ? errId : props.help ? helpId : undefined),
    invalid: () => !!props.error,
  };
  return (
    <div class={`f${props.class ? ` ${props.class}` : ""}`}>
      <div class="f-l">
        <label class={`f-lbl${props.plainLabel ? " plain" : ""}`} for={id}>
          {props.label}
        </label>
        <Show when={props.required}>
          <span class="f-req" aria-hidden="true">*</span>
          <span class="sr-only">required</span>
        </Show>
      </div>
      <FieldContext.Provider value={ctx}>{props.children}</FieldContext.Provider>
      <Show
        when={props.error}
        fallback={
          <Show when={props.help}>
            <p class="f-help" id={helpId}>{props.help}</p>
          </Show>
        }
      >
        <p class="f-err" id={errId} role="alert">
          <Icon icon="alert-triangle" size={13} />
          <span>{props.error}</span>
        </p>
      </Show>
    </div>
  );
}

/** The id and ARIA a control inside a Field should carry. */
function useFieldAttrs(own: { id?: string; invalid?: boolean }) {
  const ctx = useContext(FieldContext);
  return {
    id: () => own.id ?? ctx?.id,
    describedBy: () => ctx?.describedBy(),
    invalid: () => own.invalid ?? ctx?.invalid() ?? false,
  };
}

type InputProps = Omit<JSX.InputHTMLAttributes<HTMLInputElement>, "onInput" | "class"> & {
  value: string;
  onValue: (v: string) => void;
  mono?: boolean;
  /** Text after the value inside the box, e.g. a unit ("seconds"). */
  suffix?: string;
  invalid?: boolean;
  class?: string;
};

export function Input(props: InputProps) {
  const [own, rest] = splitProps(props, ["value", "onValue", "mono", "suffix", "invalid", "class", "id", "disabled", "readOnly"]);
  const a = useFieldAttrs(own);
  return (
    <div
      class={`f-ctl${a.invalid() ? " is-err" : ""}${own.disabled ? " is-dis" : ""}${own.readOnly ? " is-ro" : ""}${own.class ? ` ${own.class}` : ""}`}
    >
      <input
        spellcheck={false}
        autocomplete="off"
        {...rest}
        id={a.id()}
        class={`f-inp${own.mono ? " mono" : ""}`}
        value={own.value}
        disabled={own.disabled}
        readOnly={own.readOnly}
        aria-describedby={a.describedBy()}
        aria-invalid={a.invalid() || undefined}
        onInput={(e) => own.onValue(e.currentTarget.value)}
      />
      <Show when={own.suffix}>
        <span class="f-aff">{own.suffix}</span>
      </Show>
    </div>
  );
}

export function Select(props: {
  value: string;
  onValue: (v: string) => void;
  mono?: boolean;
  disabled?: boolean;
  id?: string;
  invalid?: boolean;
  "aria-label"?: string;
  children: JSX.Element;
}) {
  const a = useFieldAttrs(props);
  return (
    <div class={`f-ctl f-sel${a.invalid() ? " is-err" : ""}${props.disabled ? " is-dis" : ""}`}>
      <select
        id={a.id()}
        class={`f-inp${props.mono ? " mono" : ""}`}
        value={props.value}
        disabled={props.disabled}
        aria-label={props["aria-label"]}
        aria-describedby={a.describedBy()}
        aria-invalid={a.invalid() || undefined}
        onChange={(e) => props.onValue(e.currentTarget.value)}
      >
        {props.children}
      </select>
      <span class="f-chev">
        <Icon icon="chevron-down" size={14} />
      </span>
    </div>
  );
}

export function Textarea(props: {
  value: string;
  onValue: (v: string) => void;
  mono?: boolean;
  rows?: number;
  placeholder?: string;
  disabled?: boolean;
  autofocus?: boolean;
  id?: string;
  invalid?: boolean;
}) {
  const a = useFieldAttrs(props);
  return (
    <div class={`f-ctl f-area${a.invalid() ? " is-err" : ""}${props.disabled ? " is-dis" : ""}`}>
      <textarea
        id={a.id()}
        class={`f-inp${props.mono ? " mono" : ""}`}
        rows={props.rows ?? 6}
        value={props.value}
        placeholder={props.placeholder}
        disabled={props.disabled}
        autofocus={props.autofocus}
        spellcheck={false}
        aria-describedby={a.describedBy()}
        aria-invalid={a.invalid() || undefined}
        onInput={(e) => props.onValue(e.currentTarget.value)}
      />
    </div>
  );
}

/** A checkbox whose label is a sentence, with an optional help line. */
export function Checkbox(props: {
  checked: boolean;
  onChecked: (v: boolean) => void;
  label: JSX.Element;
  help?: JSX.Element;
  disabled?: boolean;
  /** `warn` for a box that opts into overwriting something. */
  tone?: "warn";
}) {
  return (
    <label class={`f-chk${props.tone ? ` is-${props.tone}` : ""}${props.disabled ? " is-dis" : ""}`}>
      <input
        type="checkbox"
        checked={props.checked}
        disabled={props.disabled}
        onChange={(e) => props.onChecked(e.currentTarget.checked)}
      />
      <span class="f-box" aria-hidden="true">
        <Icon icon="check" size={11} />
      </span>
      <span class="f-chk-t">
        <span class="f-chk-l">{props.label}</span>
        <Show when={props.help}>
          <span class="f-chk-h">{props.help}</span>
        </Show>
      </span>
    </label>
  );
}
