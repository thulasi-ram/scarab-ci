// The Install library dialog: where a library is read from. "Fetch and
// review" opens the review page for that source, which fetches it and shows
// the file or the diff before anything is installed. The file and its ref
// (a branch, tag or commit) are two fields; the server reads a branch or
// tag at the commit it points to when fetched, and the review page shows
// that commit. Upgrade opens the same dialog with the installed source
// filled in: change the ref and fetch.
//
// The Name field starts as `<owner>::<repo>::<file name>`, the name the
// server gives when none is sent, and follows the File field until someone
// types a name of their own. An upgrade keeps the installed name.
import { createResource, createSignal, For, Show } from "solid-js";
import { useNavigate } from "@solidjs/router";
import { listConnections } from "../api/client";
import {
  defaultLibraryName,
  libraryNameError,
  refError,
  reviewHref,
  sourceError,
  splitSource,
  toFetchBody,
  type FetchForm,
} from "../libraries";
import Icon from "./Icon";
import Modal, { ModalSection } from "./Modal";
import { Field, Input, Select } from "./Field";

export default function InstallLibraryDialog(props: {
  org: string;
  /** Prefilled fields: an upgrade's source and name. */
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
    ...props.initial,
    name: props.initial?.name || defaultLibraryName(start.source) || "",
    source: start.source,
    ref: props.initial?.ref || start.ref,
  });
  // Whether the Name is someone's own: then the File no longer changes it.
  const [nameTyped, setNameTyped] = createSignal(!!props.initial?.name || !!props.upgrade);
  const set = (k: keyof FetchForm, v: string) => setForm((f) => ({ ...f, [k]: v }));
  // A `…@<ref>` typed or pasted into File stays there: the fetch reads the
  // ref after the `@` when Ref is empty, and the Name follows the file.
  const setSource = (v: string) => {
    setForm((f) =>
      nameTyped() ? { ...f, source: v } : { ...f, source: v, name: defaultLibraryName(splitSource(v).source) ?? "" },
    );
  };
  // An emptied Name follows the File again from its next change; sent
  // empty, the server gives the same default.
  const setName = (v: string) => {
    setNameTyped(v.trim() !== "");
    set("name", v);
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
      title={props.upgrade ? `Upgrade lib:${form().name}` : "Install library"}
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
      <ModalSection n={2} title="Name">
        <Field
          label="Name"
          error={props.upgrade ? null : libraryNameError(form().name.trim())}
          help={
            props.upgrade
              ? "An upgrade keeps the installed name."
              : "Parts of letters, digits, ., _ and -, joined by :: or /. Default: <owner>::<repo>::<file name>."
          }
        >
          <Input
            mono
            readOnly={props.upgrade}
            placeholder={defaultLibraryName(splitSource(form().source).source) ?? "<owner>::<repo>::<file name>"}
            value={form().name}
            onValue={setName}
          />
        </Field>
        <p class="f-help">
          Pipelines write <span class="mono">invoke: lib:{form().name.trim() || "<name>"}</span>.
        </p>
      </ModalSection>
      <Show
        when={
          built().error &&
          form().source.trim() !== "" &&
          form().ref.trim() !== "" &&
          !sourceError(form().source) &&
          !refError(form().ref) &&
          !libraryNameError(form().name.trim())
        }
      >
        <p class="error">{built().error}</p>
      </Show>
    </Modal>
  );
}
