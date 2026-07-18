// Debug shell — an interactive TTY into a RUNNING step's Pod (the debug
// surface). Opens a WebSocket to the server's attach endpoint and bridges it to
// an xterm terminal: keystrokes → the Pod's shell stdin, shell output → the
// terminal. Only meaningful while the step is running (a terminal step's Pod is
// gone). Rendered as a modal overlay.
import { onMount, onCleanup } from "solid-js";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";

export default function DebugShell(props: {
  runId: string;
  step: string;
  onClose: () => void;
}) {
  let host: HTMLDivElement | undefined;
  let term: Terminal | undefined;
  let ws: WebSocket | undefined;

  onMount(() => {
    const t = new Terminal({
      fontFamily: '"JetBrains Mono", ui-monospace, Menlo, monospace',
      fontSize: 12.5,
      cursorBlink: true,
      theme: { background: "#0e1a14", foreground: "#cfe2d6", cursor: "#2ea77f" },
    });
    const fit = new FitAddon();
    t.loadAddon(fit);
    t.open(host!);
    fit.fit();
    term = t;

    const proto = location.protocol === "https:" ? "wss" : "ws";
    const url = `${proto}://${location.host}/v1/runs/${encodeURIComponent(props.runId)}/steps/${encodeURIComponent(props.step)}/attach`;
    const sock = new WebSocket(url);
    sock.binaryType = "arraybuffer";
    ws = sock;

    t.writeln(`\x1b[2m connecting to ${props.step}…\x1b[0m`);
    sock.onopen = () => t.writeln("\x1b[2m attached — type below (Ctrl-D to end)\x1b[0m\r\n");
    sock.onmessage = (e) => {
      const data =
        typeof e.data === "string" ? e.data : new TextDecoder().decode(new Uint8Array(e.data));
      t.write(data);
    };
    sock.onclose = () => t.writeln("\r\n\x1b[2m — shell closed —\x1b[0m");
    sock.onerror = () =>
      t.writeln("\r\n\x1b[31m — connection error (is the step still running?) —\x1b[0m");

    // Keystrokes → the Pod's shell stdin.
    t.onData((d) => {
      if (sock.readyState === WebSocket.OPEN) sock.send(d);
    });

    const onResize = () => fit.fit();
    window.addEventListener("resize", onResize);
    onCleanup(() => window.removeEventListener("resize", onResize));
  });

  onCleanup(() => {
    ws?.close();
    term?.dispose();
  });

  return (
    <div class="shell-overlay" onClick={props.onClose}>
      <div class="shell-modal" onClick={(e) => e.stopPropagation()}>
        <div class="shell-h">
          <span class="mono">⌗ debug shell · {props.step}</span>
          <span class="shell-sub mono">interactive · Administer-gated</span>
          <button class="shell-close" onClick={props.onClose} title="close">
            ✕
          </button>
        </div>
        <div class="shell-term" ref={host} />
      </div>
    </div>
  );
}
