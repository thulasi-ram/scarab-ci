// Account → Appearance: the palette (Emerald or Mono) and the mode (light or
// dark), two independent choices kept in this browser. The user menu's
// light/dark row sets the same mode.
//
// Each choice shows a small swatch. A swatch carries its own data-palette and
// data-theme, so the token blocks in styles.css paint it in the look it
// offers, whatever the page is in now.
import { For } from "solid-js";
import { PALETTES, type Palette } from "../appearance";
import { palette, setPalette, setTheme, theme, type Theme } from "../theme";

const MODES: readonly { id: Theme; label: string; sub: string }[] = [
  { id: "light", label: "Light", sub: "light surfaces, the default" },
  { id: "dark", label: "Dark", sub: "dark surfaces, light text" },
];

export default function AppearanceSettings() {
  return (
    <>
      <div class="panel">
        <div class="panel-h">
          <span>Palette</span>
        </div>
        <div class="settings-body">
          <div class="appearance-choices" role="radiogroup" aria-label="Palette">
            <For each={PALETTES}>
              {(p) => (
                <Choice
                  name="palette"
                  label={p.label}
                  sub={p.sub}
                  checked={palette() === p.id}
                  onPick={() => setPalette(p.id)}
                  palette={p.id}
                  theme={theme()}
                />
              )}
            </For>
          </div>
        </div>
      </div>

      <div class="panel">
        <div class="panel-h">
          <span>Mode</span>
        </div>
        <div class="settings-body">
          <div class="appearance-choices" role="radiogroup" aria-label="Mode">
            <For each={MODES}>
              {(m) => (
                <Choice
                  name="mode"
                  label={m.label}
                  sub={m.sub}
                  checked={theme() === m.id}
                  onPick={() => setTheme(m.id)}
                  palette={palette()}
                  theme={m.id}
                />
              )}
            </For>
          </div>
          <p class="subtle">
            <small>Both choices are kept in this browser only.</small>
          </p>
        </div>
      </div>
    </>
  );
}

function Choice(props: {
  name: string;
  label: string;
  sub: string;
  checked: boolean;
  onPick: () => void;
  palette: Palette;
  theme: Theme;
}) {
  return (
    <label class="appearance-choice" classList={{ on: props.checked }}>
      <Swatch palette={props.palette} theme={props.theme} />
      <span class="appearance-choice-row">
        <input
          type="radio"
          name={props.name}
          checked={props.checked}
          onChange={() => props.onPick()}
        />
        <span class="appearance-choice-label">{props.label}</span>
      </span>
      <span class="appearance-choice-sub">{props.sub}</span>
    </label>
  );
}

/** A miniature page: top bar, a card with two text lines, an accent bar and
 *  the four status colors (success, running, waiting, failure). */
function Swatch(props: { palette: Palette; theme: Theme }) {
  return (
    <span class="swatch" data-palette={props.palette} data-theme={props.theme} aria-hidden="true">
      <span class="swatch-bar" />
      <span class="swatch-card">
        <span class="swatch-line" />
        <span class="swatch-line short" />
        <span class="swatch-foot">
          <span class="swatch-accent" />
          <span class="swatch-dots">
            <i class="ok" />
            <i class="running" />
            <i class="warn" />
            <i class="danger" />
          </span>
        </span>
      </span>
    </span>
  );
}
