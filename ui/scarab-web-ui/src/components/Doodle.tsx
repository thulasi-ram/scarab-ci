// Background doodle: a Lucide icon re-drawn DOTTED, per docs/DESIGN.md §5 —
// zero-length dashes with round caps turn every stroke into a run of round
// dots, the same dot-matrix language as the pixel display face, the page
// dot-grid texture, and the ASCII beetle scenes. Placed 1–2 per page, rotated/
// scaled/faint, in the background — never on a control.
import { For } from "solid-js";
import { Dynamic } from "solid-js/web";
import { iconNode } from "../icons";

// Canonical doodle ink. `stroke` is a literal hex (not `var(--copper)`): it's
// an SVG presentation attribute, where CSS custom properties don't resolve.
// dasharray "0 1.2" + round caps = dots Ø strokeWidth at a 1.2-unit pitch —
// the same grain as the 10 Pixel display face.
const STROKE = "#c0873f";
const STROKE_WIDTH = 0.6;
const DASH = "0 1.2";

export default function Doodle(props: {
  icon: string;
  size?: number;
  rotate?: number;
  opacity?: number;
  top?: string;
  right?: string;
  bottom?: string;
  left?: string;
}) {
  const node = () => iconNode(props.icon) ?? [];
  const size = () => props.size ?? 200;
  return (
    <svg
      class="doodle"
      viewBox="0 0 24 24"
      width={size()}
      height={size()}
      fill="none"
      stroke={STROKE}
      stroke-width={STROKE_WIDTH}
      stroke-dasharray={DASH}
      stroke-linecap="round"
      stroke-linejoin="round"
      aria-hidden="true"
      style={{
        top: props.top,
        right: props.right,
        bottom: props.bottom,
        left: props.left,
        opacity: String(props.opacity ?? 0.08),
        transform: `rotate(${props.rotate ?? 0}deg)`,
      }}
    >
      <For each={node()}>
        {([tag, attrs]) => <Dynamic component={tag} {...attrs} />}
      </For>
    </svg>
  );
}
