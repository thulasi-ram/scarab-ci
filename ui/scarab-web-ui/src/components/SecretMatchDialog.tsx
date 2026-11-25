// "Check a leaked value" (Org settings › Secrets). An org Admin pastes text
// found outside Scarab; the server compares it with every current secret value
// of the org, its repositories and its connections, and answers with the names
// that match. Each check asks for its own passkey confirmation: the client's
// step-up retry opens the passkey dialog on the way. When the org's leak-check
// limits turn the check off, the dialog says so in place of the box.
//
// The pasted text lives in this component's state only. It is cleared on
// "Check another", and goes with the component when the dialog closes; nothing
// here writes it anywhere else.
import { createResource, createSignal, For, Show } from "solid-js";
import { A } from "@solidjs/router";
import { checkSecretMatch, getLeakCheckLimits, SecretMatchRefusedError } from "../api/client";
import { leakChecksOff, leakChecksOffText } from "../leak-checks";
import {
  byteCount,
  byteLabel,
  canCheck,
  matchName,
  MAX_MATCH_BYTES,
  resultHeading,
  searchedText,
  usageLine,
  whereText,
  type SecretMatch,
  type SecretMatchAnswer,
} from "../secret-match";
import Icon from "./Icon";
import Modal, { ModalMessage } from "./Modal";
import { Field, Textarea } from "./Field";

export default function SecretMatchDialog(props: {
  org: string;
  onClose: () => void;
  /** Open an org key in the list behind the dialog. */
  onOpenKey: (key: string, environment?: string) => void;
  /** Whether the list behind the dialog shows this key: "open" is offered only then. */
  hasKey: (key: string) => boolean;
}) {
  const [text, setText] = createSignal("");
  const [answer, setAnswer] = createSignal<SecretMatchAnswer | null>(null);
  const [busy, setBusy] = createSignal(false);
  const [refusal, setRefusal] = createSignal<string | null>(null);
  // The org's limits: `null` when they could not be read, and the server
  // still refuses a check the limits do not allow.
  const [limits] = createResource(() => props.org, getLeakCheckLimits);
  const off = () => {
    const l = limits();
    return !!l && leakChecksOff(l);
  };

  const tooLong = () => byteCount(text()) > MAX_MATCH_BYTES;

  const close = () => {
    setText("");
    setAnswer(null);
    props.onClose();
  };
  const again = () => {
    setText("");
    setAnswer(null);
    setRefusal(null);
  };
  const check = async () => {
    if (!canCheck(text())) return;
    setBusy(true);
    setRefusal(null);
    try {
      const a = await checkSecretMatch(props.org, text());
      setText("");
      setAnswer(a);
    } catch (e) {
      setRefusal(
        e instanceof SecretMatchRefusedError || e instanceof Error ? e.message : "The check failed.",
      );
    } finally {
      setBusy(false);
    }
  };
  const open = (m: SecretMatch) => {
    if (m.kind !== "secret") return;
    props.onOpenKey(m.key, m.scope === "org_environment" ? (m.environment ?? undefined) : undefined);
    close();
  };

  const Row = (p: { m: SecretMatch }) => {
    const use = () => usageLine(p.m);
    const repoHref = () =>
      p.m.kind === "secret" && p.m.repo
        ? `/${encodeURIComponent(props.org)}/${encodeURIComponent(p.m.repo)}?tab=secrets`
        : null;
    // An org-level key opens in the list behind the dialog, so only when the list shows it.
    const canOpen = () => p.m.kind === "secret" && (repoHref() !== null || props.hasKey(p.m.key));
    const openLabel = () => `Open ${matchName(p.m)}, ${whereText(p.m)}`;
    return (
      <li class="smatch">
        <div class="smatch-h">
          <code class="smatch-key">{matchName(p.m)}</code>
          <span class="smatch-where">
            <Show when={p.m.kind === "connection"}>
              <Icon icon="plug" size={13} />
            </Show>
            {whereText(p.m)}
          </span>
        </div>
        <p class="smatch-use" classList={{ warn: use().warn }}>
          {use().text}
        </p>
        <Show when={canOpen()}>
          <span class="smatch-open">
            <Show
              when={repoHref()}
              fallback={
                <button
                  class="btn btn-ghost btn-sm"
                  type="button"
                  aria-label={openLabel()}
                  onClick={() => open(p.m)}
                >
                  open
                </button>
              }
            >
              {(href) => (
                <A class="btn btn-ghost btn-sm" href={href()} aria-label={openLabel()} onClick={close}>
                  open
                </A>
              )}
            </Show>
          </span>
        </Show>
      </li>
    );
  };

  return (
    <Modal
      eyebrow={`${props.org} · secrets`}
      icon="search"
      title="Check a leaked value"
      purpose="Find which current values occur in text found outside Scarab."
      busy={busy()}
      onClose={close}
      footer={
        <>
          <Show when={refusal()}>
            <ModalMessage>{refusal()}</ModalMessage>
          </Show>
          <Show
            when={answer()}
            fallback={
              <Show
                when={!off()}
                fallback={
                  <button class="btn btn-primary" type="button" onClick={close}>
                    Close
                  </button>
                }
              >
                <button class="btn btn-ghost" type="button" onClick={close} disabled={busy()}>
                  Cancel
                </button>
                <button
                  class="btn btn-primary"
                  type="button"
                  disabled={busy() || !canCheck(text())}
                  onClick={() => void check()}
                >
                  <Icon icon="search" size={14} /> {busy() ? "Checking…" : "Check"}
                </button>
              </Show>
            }
          >
            <button class="btn btn-ghost" type="button" onClick={again}>
              Check another
            </button>
            <button class="btn btn-primary" type="button" onClick={close}>
              Done
            </button>
          </Show>
        </>
      }
    >
      <Show
        when={answer()}
        fallback={
          <Show
            when={!off()}
            fallback={
              <div class="dlg-text">
                <p>{leakChecksOffText(props.org)}</p>
              </div>
            }
          >
            <div class="dlg-text">
              <p>
                Paste what you found outside Scarab: a token, an .env file, a log excerpt. Scarab
                compares it with every current secret value in <span class="mono">{props.org}</span>,
                its repositories and its connections, and lists the ones that match. What you paste
                is not stored or logged.
              </p>
              <p>
                Only current values are compared. A value you replaced earlier is not kept, so it
                cannot match.
              </p>
              <p>
                Each check asks for a passkey confirmation.
                <Show when={limits()}>
                  {(l) => (
                    <>
                      {" "}
                      You can run {l().per_hour} an hour and {l().per_day} a day in{" "}
                      <span class="mono">{props.org}</span>.
                    </>
                  )}
                </Show>
              </p>
            </div>
            <Field
              label="Text to check"
              help={byteLabel(text())}
              error={tooLong() ? `${byteLabel(text())}: longer than the limit` : null}
            >
              <Textarea
                mono
                autofocus
                rows={8}
                placeholder="DEPLOY_TOKEN=…"
                value={text()}
                onValue={setText}
                disabled={busy()}
              />
            </Field>
          </Show>
        }
      >
        {(a) => (
          <Show
            when={a().matches.length > 0}
            fallback={
              <div class="dlg-text">
                <p>
                  <b>{resultHeading(a())}</b>
                </p>
                <p>
                  {searchedText(a())} Scarab does not keep old values, so a value you replaced
                  earlier cannot match.
                </p>
              </div>
            }
          >
            <div class="dlg-text">
              <p>
                <b>{resultHeading(a())}</b>
              </p>
              <p>{searchedText(a())}</p>
            </div>
            <ul class="smatch-list">
              <For each={a().matches}>{(m) => <Row m={m} />}</For>
            </ul>
            <p class="dlg-text">
              Rotate each credential where it was issued, then overwrite it here. This check is
              recorded in the org's events with the names above, not the text.
            </p>
          </Show>
        )}
      </Show>
    </Modal>
  );
}
