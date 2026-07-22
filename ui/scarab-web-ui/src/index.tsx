// Entry point. Mounts the app; a bundler (Vite) is wired with the full UI.
import { render } from "solid-js/web";

// Self-hosted typefaces (docs/DESIGN.md §3): Space Grotesk for display, Inter for
// body/UI, JetBrains Mono for machine tokens/labels. Then the design stylesheet.
// Display voice (DESIGN.md §5): Space Grotesk — headings + wordmark.
import "@fontsource/space-grotesk/500.css";
import "@fontsource/space-grotesk/700.css";
import "@fontsource/inter/400.css";
import "@fontsource/inter/500.css";
import "@fontsource/inter/600.css";
import "@fontsource/jetbrains-mono/400.css";
import "@fontsource/jetbrains-mono/500.css";
import "./styles.css";

// Fixture/demo mode (`just ui-mock`): VITE_SCARAB_MOCK=1 serves an "acme" org
// with no server — quick UI eyeballing + the docs screenshots. Install BEFORE
// importing App: the api client (openapi-fetch) binds globalThis.fetch when
// its module is first evaluated, so the fetch/EventSource patch must be in
// place before App (→ src/api/client.ts) loads. The dynamic imports also keep
// src/mock.ts out of the production bundle when the flag is unset.
if (import.meta.env.VITE_SCARAB_MOCK === "1") {
  const { installMock } = await import("./mock");
  installMock();
}

const { default: App } = await import("./App");

const root = document.getElementById("root");
if (root) {
  render(() => <App />, root);
}
