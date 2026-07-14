// Crisp functional Lucide icon: 1.5px stroke, currentColor, no fill — for real UI
// (nav, buttons, labels). The hand-drawn variant lives in Doodle.tsx. Renders the
// shared IconNode data (see ../icons) straight to SVG primitives.
import { For } from "solid-js";
import { Dynamic } from "solid-js/web";
import { iconNode } from "../icons";

export default function Icon(props: { icon: string; size?: number; class?: string }) {
  const node = () => iconNode(props.icon) ?? [];
  const size = () => props.size ?? 18;
  return (
    <svg
      width={size()}
      height={size()}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="1.5"
      stroke-linecap="round"
      stroke-linejoin="round"
      class={props.class}
      aria-hidden="true"
    >
      <For each={node()}>{([tag, attrs]) => <Dynamic component={tag} {...attrs} />}</For>
    </svg>
  );
}
