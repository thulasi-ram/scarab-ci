// State tags on a list row or beside a heading: "yanked", "untrusted
// source", "killed", "probe failed". Each is a small dotted tag in its
// tone's colour, with a title that says what it means.
import { For } from "solid-js";
import type { Chip } from "../libraries";

export default function Chips(props: { chips: readonly Chip[] }) {
  return (
    <span class="chips">
      <For each={props.chips}>
        {(c) => (
          <span class={`tok-state chip-${c.tone}`} title={c.title}>
            {c.label}
          </span>
        )}
      </For>
    </span>
  );
}
