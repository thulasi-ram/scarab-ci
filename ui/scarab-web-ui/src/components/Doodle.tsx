// Background doodle — docs/DESIGN.md §5.
// Serves a committed, pre-generated DOT-MATRIX icon SVG (baked by
// ui/brand/ascii — the same pipeline as the ASCII beetle scenes, so both UIs
// share identical assets): the icon's strokes rasterized onto a dot grid,
// several dots wide. Placed 1–2 per page, rotated/scaled/faint, in the
// background — never on a control. No generator code ships.
const svgs = import.meta.glob("../../../brand/ascii/generated/doodles/*.svg", {
  query: "?raw",
  import: "default",
  eager: true,
}) as Record<string, string>;

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
  const raw = () => {
    const entry = Object.entries(svgs).find(([p]) => p.endsWith(`/${props.icon}.svg`));
    if (!entry) throw new Error(`Doodle: unknown motif "${props.icon}" (run npm run bake in ui/brand/ascii)`);
    return entry[1];
  };
  const size = () => props.size ?? 200;
  return (
    <span
      class="doodle"
      aria-hidden="true"
      innerHTML={raw()}
      style={{
        width: `${size()}px`,
        height: `${size()}px`,
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
