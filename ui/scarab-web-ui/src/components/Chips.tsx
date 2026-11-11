// State tags on a list row or beside a heading: "yanked", "untrusted
// source", "killed", "probe failed". Each is a small dotted tag in its
// tone's colour, with a title that says what it means.
import { For } from "solid-js";
import type { Chip } from "../libraries";
import Pill from "./Pill";

export default function Chips(props: { chips: readonly Chip[] }) {
  return (
    <span class="chips">
      <For each={props.chips}>
        {(c) => (
          <Pill tone={c.tone} look="dotted" title={c.title}>
            {c.label}
          </Pill>
        )}
      </For>
    </span>
  );
}
