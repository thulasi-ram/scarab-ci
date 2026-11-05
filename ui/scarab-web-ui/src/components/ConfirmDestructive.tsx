// The confirmation for an action that removes or revokes something. You type
// the thing's name to enable the button, so a stray click or Enter cannot
// remove it. It replaces the browser's confirm() and prompt(): those cannot
// be styled, and embedded viewers block them.
//
// `onConfirm` does the work. If it throws, the dialog stays open and shows
// the error as the server gave it; if it returns, the dialog closes.
import { createSignal, Show, type JSX } from "solid-js";
import { confirmsName } from "../org-environments";
import { Field, Input } from "./Field";
import Modal, { ModalMessage } from "./Modal";
import Icon from "./Icon";

export default function ConfirmDestructive(props: {
  /** A question: "Remove scarab's overlay on production?" */
  title: JSX.Element;
  /** One line: what happens as a result. */
  purpose?: JSX.Element;
  eyebrow?: JSX.Element;
  /** Anything more the reader needs before confirming. */
  children?: JSX.Element;
  /** What must be typed to confirm. */
  name: string;
  /** The button's label: "Remove overlay". */
  confirmLabel: string;
  onConfirm: () => Promise<void> | void;
  onClose: () => void;
}) {
  const [typed, setTyped] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const matches = () => confirmsName(typed(), props.name);

  const go = async () => {
    if (!matches() || busy()) return;
    setBusy(true);
    setError(null);
    try {
      await props.onConfirm();
      props.onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : "The server refused it.");
      setBusy(false);
    }
  };

  return (
    <Modal
      size="narrow"
      title={props.title}
      purpose={props.purpose}
      eyebrow={props.eyebrow}
      busy={busy()}
      onClose={props.onClose}
      footer={
        <>
          <Show when={error()}>
            <ModalMessage>{error()}</ModalMessage>
          </Show>
          <button class="btn btn-ghost" type="button" onClick={props.onClose} disabled={busy()}>
            Cancel
          </button>
          <button class="btn btn-destroy" type="button" disabled={!matches() || busy()} onClick={go}>
            <Icon icon="trash-2" size={14} />
            {props.confirmLabel}
          </button>
        </>
      }
    >
      <Show when={props.children}>
        <div class="dlg-text">{props.children}</div>
      </Show>
      <Field
        plainLabel
        label={
          <>
            Type <b class="mono">{props.name}</b> to confirm
          </>
        }
      >
        <Input
          mono
          autofocus
          value={typed()}
          onValue={setTyped}
          onKeyDown={(e) => {
            if (e.key === "Enter") void go();
          }}
        />
      </Field>
    </Modal>
  );
}
