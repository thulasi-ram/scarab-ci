// The one pill: every status, state, role and kind tag in the web UI. Its text
// is always the Mono style on a 16px line, inside 1px padding and a 1px border,
// so every pill is 20px tall and none reads louder than the line beside it
// (the `.pill` rule in styles.css). A pill differs from another only in:
//
// - `tone`, its colour: ok, danger, motion (in progress), muted, accent,
//   copper, warn, or plain (the secondary text colour);
// - `look`: `outline` (a border in the tone, the default), `solid` (filled:
//   run and attempt statuses), `dotted` (a state that qualifies a row), `dashed`
//   (a warning about the thing itself), or `tint` (a light fill, no border).
import type { ParentProps } from "solid-js";

export type PillTone = "ok" | "danger" | "motion" | "muted" | "accent" | "copper" | "warn" | "plain";
export type PillLook = "outline" | "solid" | "dotted" | "dashed" | "tint";

export default function Pill(
  props: ParentProps<{ tone?: PillTone; look?: PillLook; title?: string; class?: string }>,
) {
  return (
    <span
      class={`pill pill-${props.tone ?? "plain"} pill-${props.look ?? "outline"}${props.class ? ` ${props.class}` : ""}`}
      title={props.title}
    >
      {props.children}
    </span>
  );
}
