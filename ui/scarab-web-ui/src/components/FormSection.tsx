// A numbered section of a form: the Environment edit page and every dialog
// that asks three or more questions.
import { Show, type JSX } from "solid-js";

export default function FormSection(props: {
  n: number;
  title: string;
  note?: string;
  children: JSX.Element;
}) {
  return (
    <section class="env-sec">
      <h2 class="env-sec-h">
        <span class="env-sec-n mono">{props.n}</span>
        {props.title}
      </h2>
      <Show when={props.note}>
        <p class="fieldnote">{props.note}</p>
      </Show>
      <div class="env-sec-body">{props.children}</div>
    </section>
  );
}
