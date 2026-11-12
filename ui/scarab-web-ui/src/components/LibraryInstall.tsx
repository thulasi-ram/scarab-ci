// The Install library dialog: where a library is read from. "Fetch and
// review" opens the review page for that source, which fetches it and shows
// the file or the diff before anything is installed. The file and its ref
// (a branch, tag or commit) are two fields; the server reads a branch or
// tag at the commit it points to when fetched, and the review page shows
// that commit. Upgrade opens the same dialog with the installed source
// filled in: change the ref and fetch.
import { createResource, createSignal, For, Show } from "solid-js";
import { useNavigate } from "@solidjs/router";
import { listConnections } from "../api/client";
import { refError, reviewHref, sourceError, splitSource, toFetchBody, type FetchForm } from "../libraries";
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
  // An installed source arrives as `…@<commit>`: the commit goes in Ref.
  const start = splitSource(props.initial?.source ?? "");
  const [form, setForm] = createSignal<FetchForm>({
    connection: "",
    publisher: "",
    name: "",
    ...props.initial,
    source: start.source,
    ref: props.initial?.ref || start.ref,
  });
  const set = (k: keyof FetchForm, v: string) => setForm((f) => ({ ...f, [k]: v }));
  // A pasted `…@<ref>` fills both fields.
  const setSource = (v: string) => {
    const s = splitSource(v);
    setForm((f) => (s.ref ? { ...f, source: s.source, ref: s.ref } : { ...f, source: v }));
  };
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
          ? "Give the branch, tag or commit to upgrade to. The next page shows the diff from the installed version."
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
          label="File"
          required
          error={sourceError(form().source)}
          help="The repository, then // and the file's path in it. A public repository on github.com or a Forgejo connection's host is read without credentials."
        >
          <Input
            mono
            autofocus={!props.upgrade}
            placeholder="github.com/acme/scarab-libs//deploy-helm.yaml"
            value={form().source}
            onValue={setSource}
          />
        </Field>
        <Field
          label="Ref"
          required
          error={refError(form().ref)}
          help="A branch, tag or commit. A branch or tag is read at the commit it points to now; the installed version stays at that commit."
        >
          <Input
            mono
            autofocus={props.upgrade}
            placeholder="main, v1.2.0 or a commit id"
            value={form().ref}
            onValue={(v) => set("ref", v)}
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
      <Show
        when={
          built().error &&
          form().source.trim() !== "" &&
          form().ref.trim() !== "" &&
          !sourceError(form().source) &&
          !refError(form().ref)
        }
      >
        <p class="error">{built().error}</p>
      </Show>
    </Modal>
  );
}
