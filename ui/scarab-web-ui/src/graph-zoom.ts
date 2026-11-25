// The run graph's zoom, remembered in this browser: the graph opens at the
// zoom the viewer last chose with ＋/－, on any run. The reset button forgets
// it, so the graph opens at 100% again. Storage can be missing or refuse (a
// private window, blocked site data); the graph then opens at 100%.

const KEY = "scarab-graph-zoom";
export const MIN_ZOOM = 0.2;
export const MAX_ZOOM = 1.5;

type Store = Pick<Storage, "getItem" | "setItem" | "removeItem">;
const store = (): Store | null => {
  try {
    return globalThis.localStorage ?? null;
  } catch {
    return null;
  }
};

/** The remembered zoom, or null when none is stored or it is not a zoom. */
export function readGraphZoom(s: Store | null = store()): number | null {
  try {
    const raw = s?.getItem(KEY);
    if (raw == null) return null;
    const z = Number(raw);
    return Number.isFinite(z) && z >= MIN_ZOOM && z <= MAX_ZOOM ? z : null;
  } catch {
    return null;
  }
}

export function writeGraphZoom(z: number, s: Store | null = store()): void {
  try {
    s?.setItem(KEY, String(Math.round(z * 1000) / 1000));
  } catch {
    /* storage refused: the zoom lasts for this page only */
  }
}

export function clearGraphZoom(s: Store | null = store()): void {
  try {
    s?.removeItem(KEY);
  } catch {
    /* nothing stored, or storage refused */
  }
}
