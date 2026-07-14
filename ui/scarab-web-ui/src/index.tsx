// Entry point. Mounts the app; a bundler (Vite) is wired with the full UI.
import { render } from "solid-js/web";
import App from "./App";

// Self-hosted typefaces (docs/DESIGN.md §3): Space Grotesk for display, Inter for
// body/UI, JetBrains Mono for machine tokens/labels. Then the design stylesheet.
import "@fontsource/space-grotesk/500.css";
import "@fontsource/space-grotesk/700.css";
import "@fontsource/inter/400.css";
import "@fontsource/inter/500.css";
import "@fontsource/inter/600.css";
import "@fontsource/jetbrains-mono/400.css";
import "@fontsource/jetbrains-mono/500.css";
import "./styles.css";

const root = document.getElementById("root");
if (root) {
  render(() => <App />, root);
}
