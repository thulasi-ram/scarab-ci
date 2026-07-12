// Entry point. Mounts the app; a bundler (Vite) is wired with the full UI.
import { render } from "solid-js/web";
import App from "./App";

const root = document.getElementById("root");
if (root) {
  render(() => <App />, root);
}
