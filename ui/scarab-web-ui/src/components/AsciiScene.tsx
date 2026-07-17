// Animated brand beetle — plays a baked ASCII scene from ui/brand/ascii (see
// its README). State moments ONLY (all-clear, empty, loading) — never ambient
// behind live data. No rendering code: the loop swaps pre-baked text frames at
// the scene's fps; three <pre> layers are colored via the --ascii-* tokens.
import { onCleanup, onMount } from "solid-js";

// frames: per frame, three text layers (emerald, gold, gray) — typed loosely
// because resolveJsonModule infers string[][] from the baked files.
type Baked = {
  cols: number;
  rows: number;
  fps: number;
  frames: string[][];
};

export default function AsciiScene(props: {
  scene: Baked;
  /** px per cell column; glyph advance is ~0.602 × this */
  fontSize?: number;
  /** accessible name; omit → decorative (aria-hidden) */
  label?: string;
  class?: string;
}) {
  const pres: HTMLPreElement[] = [];
  const { frames, fps } = props.scene;

  onMount(() => {
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
      // Hold a fully-open frame instead of animating (frame 0 is closed).
      const mid = frames[Math.floor(frames.length / 2)];
      pres.forEach((p, i) => (p.textContent = mid[i]));
      return;
    }
    let f = 0;
    const t = setInterval(() => {
      if (document.hidden) return;
      f = (f + 1) % frames.length;
      for (let i = 0; i < 3; i++) pres[i].textContent = frames[f][i];
    }, 1000 / fps);
    onCleanup(() => clearInterval(t));
  });

  return (
    <div
      class={`ascii-scene ${props.class ?? ""}`}
      style={{ "--ascii-fs": `${props.fontSize ?? 8}px` }}
      role={props.label ? "img" : undefined}
      aria-label={props.label}
      aria-hidden={props.label ? undefined : "true"}
    >
      <pre class="ascii-em" ref={(el) => (pres[0] = el)}>{frames[0][0]}</pre>
      <pre class="ascii-au" ref={(el) => (pres[1] = el)}>{frames[0][1]}</pre>
      <pre class="ascii-fe" ref={(el) => (pres[2] = el)}>{frames[0][2]}</pre>
    </div>
  );
}
