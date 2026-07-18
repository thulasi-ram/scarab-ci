// Shared open-state for the ⌘K command palette, so the header search chip and
// the global keyboard shortcut both drive the one modal.
import { createSignal } from "solid-js";

export const [paletteOpen, setPaletteOpen] = createSignal(false);
