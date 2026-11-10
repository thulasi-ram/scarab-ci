// The one dialog frame. Every dialog in the app is a Modal, so every dialog
// has the same header, body, footer and one of three widths:
//
//   narrow   440px  a confirmation: one question, one or two buttons
//   standard 560px  a form
//   wide     760px  a review: a list, a diff, a summary of changes
//
// No screen sets its own width. A form that outgrows `standard` belongs on a
// page of its own (as the library install review is), not in a taller dialog.
//
// The header is a 20px title and one line saying what the dialog is for. A
// body with three or more questions splits them into numbered ModalSections.
// The footer holds the actions and, when given, an `after` line that says
// what applies once the dialog is saved.
//
// The dialog is portalled to <body>, closes on Escape and on a click on the
// scrim (both ignored while `busy`), and takes focus on open: the control
// marked `autofocus`, or else the dialog.
import { createUniqueId, onCleanup, onMount, Show, type JSX } from "solid-js";
import { Portal } from "solid-js/web";
import Icon from "./Icon";

export type ModalSize = "narrow" | "standard" | "wide";

export default function Modal(props: {
  title: JSX.Element;
  /** One line: what this dialog is for. */
  purpose?: JSX.Element;
  /** A short mono line above the title: where this dialog acts. */
  eyebrow?: JSX.Element;
  icon?: string;
  size?: ModalSize;
  onClose: () => void;
  /** While set, Escape and the scrim do not close the dialog. */
  busy?: boolean;
  /** What applies after saving, shown above the footer's actions. */
  after?: { label?: string; value: JSX.Element };
  /** Footer content: messages and buttons, in order. */
  footer: JSX.Element;
  children: JSX.Element;
}) {
  const titleId = createUniqueId();
  let dialog: HTMLDivElement | undefined;
  let downOnScrim = false;
  const close = () => {
    if (!props.busy) props.onClose();
  };

  onMount(() => {
    const opener = document.activeElement as HTMLElement | null;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") close();
    };
    document.addEventListener("keydown", onKey);
    onCleanup(() => {
      document.removeEventListener("keydown", onKey);
      opener?.focus?.();
    });
    // A control marked `autofocus` takes focus; otherwise the dialog itself
    // does, so Tab starts at its first control and a screen reader reads the
    // title first.
    (dialog?.querySelector<HTMLElement>("[autofocus]") ?? dialog)?.focus();
  });

  return (
    <Portal>
      <div
        class="dlg-scrim"
        onMouseDown={(e) => (downOnScrim = e.target === e.currentTarget)}
        onClick={(e) => {
          // Only a press that started on the scrim closes: a drag that began
          // in a field and ended outside the dialog must not.
          if (downOnScrim && e.target === e.currentTarget) close();
        }}
      >
        <div
          ref={dialog}
          class={`dlg dlg-${props.size ?? "standard"}`}
          role="dialog"
          aria-modal="true"
          aria-labelledby={titleId}
          tabindex="-1"
        >
          <div class="dlg-h">
            <Show when={props.eyebrow}>
              <div class="dlg-eb">{props.eyebrow}</div>
            </Show>
            <h2 class="dlg-t" id={titleId}>
              <Show when={props.icon}>
                <Icon icon={props.icon!} size={18} />
              </Show>
              {props.title}
            </h2>
            <Show when={props.purpose}>
              <p class="dlg-p">{props.purpose}</p>
            </Show>
            <button class="dlg-x" type="button" aria-label="Close" onClick={close} disabled={props.busy}>
              <Icon icon="x" size={16} />
            </button>
          </div>
          <div class="dlg-b">{props.children}</div>
          <div class="dlg-f">
            <Show when={props.after}>
              {(a) => (
                <div class="dlg-after">
                  <span class="k">{a().label ?? "After saving"}</span>
                  <span>{a().value}</span>
                </div>
              )}
            </Show>
            {props.footer}
          </div>
        </div>
      </div>
    </Portal>
  );
}

/** A numbered section of a dialog body: `01 Target`, `02 Approvers`, … */
export function ModalSection(props: { n: number; title: string; children: JSX.Element }) {
  return (
    <section class="dlg-sec">
      <h3 class="dlg-sec-h">
        {String(props.n).padStart(2, "0")}
        <b>{props.title}</b>
      </h3>
      <div class="dlg-sec-b">{props.children}</div>
    </section>
  );
}

/** A message line in a dialog footer: a server refusal, a validation error. */
export function ModalMessage(props: { children: JSX.Element }) {
  return (
    <p class="dlg-msg" role="alert">
      <Icon icon="alert-triangle" size={13} />
      <span>{props.children}</span>
    </p>
  );
}
