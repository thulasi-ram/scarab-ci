// A Pipeline that does not compile, as rows of a check list (ADR-0076): the
// checks it still passed, dimmed, then one row per error with a "what" line,
// a "fix" line, and a collapsed "Why" holding the explanation, a suggested
// change and the server's own message.
import { createSignal, For, Show } from "solid-js";
import {
  changeText,
  errorChange,
  errorFix,
  errorWhy,
  passedChecks,
  type CompileError,
} from "../compile-errors";
import Icon from "./Icon";

export default function CompileErrors(props: { errors: CompileError[]; refName?: string; sha?: string | null }) {
  return (
    <>
      <p class="error" style={{ margin: 0 }}>
        The Pipeline does not compile
        <Show when={props.refName}> at <span class="mono">{props.refName}</span></Show>
        <Show when={props.sha}>
          {(sha) => (
            <>
              {" "}
              (<span class="mono sha" style={{ color: "var(--copper)" }}>{sha().slice(0, 8)}</span>)
            </>
          )}
        </Show>
        , so it cannot run.
      </p>
      <ul class="ck-list">
        <For each={passedChecks(props.errors)}>
          {(t) => (
            <li class="ok">
              <Icon icon="check" size={13} />
              <span>{t}</span>
            </li>
          )}
        </For>
        <For each={props.errors}>{(e) => <ErrorRow e={e} />}</For>
      </ul>
    </>
  );
}

function ErrorRow(props: { e: CompileError }) {
  const e = () => props.e;
  const [copied, setCopied] = createSignal(false);
  const change = () => errorChange(e());
  const copy = () => {
    void navigator.clipboard?.writeText(changeText(change())).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    });
  };
  return (
    <li class="bad">
      <Icon icon="circle-x" size={14} />
      <div class="ck-main">
        <div class="ck-what">
          <Show when={e().kind !== "other"} fallback={<span>{e().message}</span>}>
            step <code class="c">{e().step}</code>
            <Show when={e().library}>
              {" "}
              <span class="sep">→</span> <code class="c">lib:{e().library}</code> step{" "}
              <code class="c">{e().library_step}</code>
            </Show>{" "}
            <span class="sep">·</span> command[{e().command_index}]{" "}
            {e().kind === "script_template" ? "runs a script with" : "reads"}{" "}
            <code class="c">{e().library ? `with.${e().input}: ${e().bound}` : e().read}</code>
          </Show>
        </div>
        <Show when={errorFix(e())}>
          {(fix) => (
            <div class="ck-fix">
              <span class="lbl">fix</span>
              <span>{fix()}</span>
              <Show when={change().length > 0}>
                <button class="xs-link" onClick={copy}>
                  {copied() ? "copied" : "copy change"}
                </button>
              </Show>
            </div>
          )}
        </Show>
        <Show when={e().kind !== "other"}>
          <details class="ck-why">
            <summary>Why</summary>
            <div class="ck-why-body">
              <Show when={errorWhy(e())}>{(w) => <p>{w()}</p>}</Show>
              <Show when={change().length > 0}>
                <div class="ud">
                  <For each={change()}>
                    {(l) => (
                      <div
                        class={`ud-row ${l.sign === "+" ? "add" : l.sign === "-" ? "del" : ""}`}
                        style={{ "grid-template-columns": "14px 1fr" }}
                      >
                        <span class="sg">{l.sign}</span>
                        <span class="tx">{l.text}</span>
                      </div>
                    )}
                  </For>
                </div>
              </Show>
              <pre>{e().message}</pre>
            </div>
          </details>
        </Show>
      </div>
    </li>
  );
}
