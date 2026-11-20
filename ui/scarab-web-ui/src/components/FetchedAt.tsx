// The ref a library version was fetched at and the commit it resolved to:
// `v1.2.0 → 1a2b3c4`, both in mono. The commit is cut to 7 characters; its
// title holds the full commit, and a click copies it. With no ref (the
// source named the commit), the commit alone. A long ref is cut with an
// ellipsis; its title holds all of it.
import { createSignal, Show } from "solid-js";
import { fetchedAt } from "../libraries";

export default function FetchedAt(props: { v: { requested_ref?: string | null; resolved_commit: string } }) {
  const f = () => fetchedAt(props.v);
  const [copied, setCopied] = createSignal(false);
  const copy = (e: MouseEvent) => {
    e.stopPropagation();
    navigator.clipboard
      ?.writeText(f().commit)
      .then(() => {
        setCopied(true);
        setTimeout(() => setCopied(false), 1500);
      })
      .catch(() => {});
  };
  return (
    <span class="lib-at" data-commit={f().commit}>
      <Show when={f().ref}>
        {(r) => (
          <>
            <span class="mono lib-ref" title={r()}>
              {r()}
            </span>
            <span class="lib-arrow"> → </span>
          </>
        )}
      </Show>
      <Show when={f().commit}>
        <button
          type="button"
          class="mono lib-commit"
          title={copied() ? `${f().commit} · copied` : `${f().commit} · click to copy`}
          aria-label={`Copy commit ${f().commit}`}
          onClick={copy}
        >
          {f().short}
        </button>
      </Show>
    </span>
  );
}
