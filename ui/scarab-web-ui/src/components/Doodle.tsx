// Background doodle: a Lucide icon re-drawn hand-sketched via rough.js, per
// docs/DESIGN.md §5. Canonical options: low roughness (barely a wobble), gentle
// curve smoothing (curveStepCount), copper ink, outlines only. Placed 1–2 per
// page, rotated/scaled/faint, in the background — never on a control.
import { onMount } from "solid-js";
import rough from "roughjs";
import type { RoughSVG } from "roughjs/bin/svg";
import { iconNode } from "../icons";

// Canonical doodle ink. `stroke` is a literal hex (not `var(--copper)`): rough.js
// writes it as an SVG presentation attribute, where CSS custom properties don't
// resolve. Smoothing is the inverse of `bowing`, so we keep bowing low and lift
// `curveStepCount` for gentle curves, with `roughness` deliberately tiny.
const OPTS = {
  roughness: 0.12,
  bowing: 0.5,
  curveStepCount: 5,
  strokeWidth: 1.6,
  stroke: "#c0873f",
  fill: "none",
} as const;

function toPairs(points: string): [number, number][] {
  const n = points.trim().split(/[\s,]+/).map(Number);
  const out: [number, number][] = [];
  for (let i = 0; i + 1 < n.length; i += 2) out.push([n[i], n[i + 1]]);
  return out;
}

function drawPrimitive(rc: RoughSVG, tag: string, a: Record<string, string | number>) {
  const num = (v: string | number | undefined) => Number(v ?? 0);
  switch (tag) {
    case "path":
      return a.d ? rc.path(String(a.d), OPTS) : null;
    case "circle":
      return rc.circle(num(a.cx), num(a.cy), num(a.r) * 2, OPTS);
    case "ellipse":
      return rc.ellipse(num(a.cx), num(a.cy), num(a.rx) * 2, num(a.ry) * 2, OPTS);
    case "line":
      return rc.line(num(a.x1), num(a.y1), num(a.x2), num(a.y2), OPTS);
    case "rect":
      return rc.rectangle(num(a.x), num(a.y), num(a.width), num(a.height), OPTS);
    case "polyline":
      return rc.linearPath(toPairs(String(a.points)), OPTS);
    case "polygon":
      return rc.polygon(toPairs(String(a.points)), OPTS);
    default:
      return null;
  }
}

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
  let svgRef: SVGSVGElement | undefined;

  onMount(() => {
    const node = iconNode(props.icon);
    if (!svgRef || !node) return;
    const rc = rough.svg(svgRef);
    for (const [tag, attrs] of node) {
      const el = drawPrimitive(rc, tag, attrs);
      if (el) svgRef.appendChild(el);
    }
  });

  const size = () => props.size ?? 200;
  return (
    <svg
      ref={svgRef}
      class="doodle"
      viewBox="0 0 24 24"
      width={size()}
      height={size()}
      aria-hidden="true"
      style={{
        top: props.top,
        right: props.right,
        bottom: props.bottom,
        left: props.left,
        opacity: String(props.opacity ?? 0.08),
        transform: `rotate(${props.rotate ?? 0}deg)`,
      }}
    />
  );
}
