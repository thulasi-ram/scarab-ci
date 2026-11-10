// The Install library dialog: where a library is read from. "Fetch and
// review" opens the review page for that source, which fetches it and shows
// the file or the diff before anything is installed. Upgrade opens the same
// dialog with the installed source filled in: change the commit and fetch.
import { createResource, createSignal, For, Show } from "solid-js";
import { useNavigate } from "@solidjs/router";
import { listConnections } from "../api/client";
import { reviewHref, sourceError, toFetchBody, type FetchForm } from "../libraries";
import Icon from "./Icon";
import Modal, { ModalSection } from "./Modal";
import { Field, Input, Select } from "./Field";

export default function InstallLibraryDialog(props: {
  org: string;
  /** Prefilled fields: an upgrade's source, publisher and name. */
  initial?: Partial<FetchForm>;
  /** Upgrading an installed library: the name is fixed. */
  upgrade?: boolean;
  onClose: () => void;
}) {
  const nav = useNavigate();
  // A library is read through a connection of the installing org only.
  const [connections] = createResource(
    () => props.org,
    (o) => listConnections(o).catch(() => []),
  );
  const [form, setForm] = createSignal<FetchForm>({
    source: "",
    connection: "",
    publisher: "",
    name: "",
    ...props.initial,
  });
  const set = (k: keyof FetchForm, v: string) => setForm((f) => ({ ...f, [k]: v }));
  const built = () => toFetchBody(form());
  const go = () => {
    const body = built().body;
    if (!body) return;
    props.onClose();
    nav(reviewHref(props.org, body));
  };

  return (
    <Modal
      eyebrow={`${props.org} · libraries`}
      title={props.upgrade ? `Upgrade lib:${form().publisher}/${form().name}` : "Install library"}
      purpose={
        props.upgrade
          ? "Give the commit to upgrade to. The next page shows the diff from the installed version."
          : "Give the file to install. The next page shows it, and nothing is installed until you confirm there."
      }
      onClose={props.onClose}
      footer={
        <>
          <button class="btn btn-ghost" type="button" onClick={props.onClose}>
            Cancel
          </button>
          <button class="btn btn-primary" type="button" disabled={!built().body} onClick={go}>
            <Icon icon="search" size={14} /> Fetch and review
          </button>
        </>
      }
    >
      <ModalSection n={1} title="Source">
        <Field
          label="Source"
          required
          error={sourceError(form().source)}
          help="A file in a git repository at a full commit id. A public repository on github.com or a Forgejo connection's host is read without credentials."
        >
          <Input
            mono
            autofocus
            placeholder="github.com/acme/scarab-libs//deploy-helm.yaml@<commit>"
            value={form().source}
            onValue={(v) => set("source", v)}
          />
        </Field>
        <Field label="Connection (private repository)">
          <Select value={form().connection} onValue={(v) => set("connection", v)}>
            <option value="">none: read it anonymously</option>
            <For each={connections() ?? []}>
              {(c) => (
                <option value={c.id}>
                  {c.id} ({c.kind}, {c.base_url})
                </option>
              )}
            </For>
          </Select>
        </Field>
      </ModalSection>
      <Show when={!props.upgrade}>
        <ModalSection n={2} title="Name (optional)">
          <div class="lib-pair">
            <Field label="Publisher" help="Default: the repository owner.">
              <Input mono placeholder="the repository owner" value={form().publisher} onValue={(v) => set("publisher", v)} />
            </Field>
            <Field label="Name" help="Default: the file name.">
              <Input mono placeholder="the file name" value={form().name} onValue={(v) => set("name", v)} />
            </Field>
          </div>
          <p class="f-help">
            Pipelines name it{" "}
            <span class="mono">
              lib:{form().publisher || "<owner>"}/{form().name || "<file>"}
            </span>
            .
          </p>
        </ModalSection>
      </Show>
      <Show when={built().error && form().source.trim() !== "" && !sourceError(form().source)}>
        <p class="error">{built().error}</p>
      </Show>
    </Modal>
  );
}
