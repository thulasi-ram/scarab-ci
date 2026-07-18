// Global command palette (⌘K / Ctrl+K): fuzzy-jump to any repo, or open a run
// by id. A client-side index over GET /v1/repos for v1 (server-side ?q= search
// is a follow-up for orgs with thousands of repos). Keyboard-first: arrows to
// move, Enter to go, Esc to close.
import { createResource, createSignal, createMemo, createEffect, For, Show, onCleanup, onMount } from "solid-js";
import { useNavigate } from "@solidjs/router";
import { listProjects, type Project } from "../api/client";
import { paletteOpen as open, setPaletteOpen as setOpen } from "../palette";
import Icon from "./Icon";

type Item =
  | { kind: "repo"; label: string; sub: string; href: string }
  | { kind: "run"; label: string; sub: string; href: string };

const RUN_ID = /^[0-9a-f]{4,}$/i;

export default function CommandPalette() {
  const [q, setQ] = createSignal("");
  const [sel, setSel] = createSignal(0);
  const [projects] = createResource(open, () => listProjects());
  const nav = useNavigate();
  let inputRef: HTMLInputElement | undefined;

  const items = createMemo<Item[]>(() => {
    const query = q().trim().toLowerCase();
    const repos = (projects() ?? [])
      .filter((p: Project) => {
        if (!query) return true;
        return `${p.org}/${p.project} ${p.owner}/${p.name}`.toLowerCase().includes(query);
      })
      .slice(0, 8)
      .map<Item>((p) => ({
        kind: "repo",
        label: `${p.org}/${p.project}`,
        sub: `${p.owner}/${p.name}`,
        href: `/${p.org}/${p.project}`,
      }));
    const out: Item[] = [...repos];
    // A hex-looking query is also a run id to jump to.
    if (RUN_ID.test(q().trim())) {
      out.unshift({
        kind: "run",
        label: `Open run ${q().trim().slice(0, 12)}`,
        sub: "jump to run by id",
        href: `/api/unknown/runs/${q().trim()}`,
      });
    }
    return out;
  });

  createEffect(() => {
    items();
    setSel(0);
  });

  function close() {
    setOpen(false);
    setQ("");
  }
  function go(item: Item | undefined) {
    if (!item) return;
    close();
    nav(item.href);
  }

  onMount(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setOpen((v) => !v);
        return;
      }
      if (!open()) return;
      if (e.key === "Escape") close();
      else if (e.key === "ArrowDown") {
        e.preventDefault();
        setSel((s) => Math.min(s + 1, items().length - 1));
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setSel((s) => Math.max(s - 1, 0));
      } else if (e.key === "Enter") {
        e.preventDefault();
        go(items()[sel()]);
      }
    };
    document.addEventListener("keydown", onKey);
    onCleanup(() => document.removeEventListener("keydown", onKey));
  });

  // Focus the input whenever the palette opens.
  createEffect(() => {
    if (open()) queueMicrotask(() => inputRef?.focus());
  });

  return (
    <Show when={open()}>
      <div class="cmdk-backdrop" onClick={close}>
        <div class="cmdk" onClick={(e) => e.stopPropagation()} role="dialog" aria-label="Command palette">
          <div class="cmdk-input">
            <Icon icon="search" size={16} />
            <input
              ref={inputRef}
              type="text"
              placeholder="Search repos, or paste a run id…"
              value={q()}
              onInput={(e) => setQ(e.currentTarget.value)}
            />
            <span class="cmdk-esc mono">esc</span>
          </div>
          <div class="cmdk-list">
            <For each={items()} fallback={<div class="cmdk-empty">No matches.</div>}>
              {(item, i) => (
                <button
                  class={`cmdk-item ${sel() === i() ? "sel" : ""}`}
                  onMouseEnter={() => setSel(i())}
                  onClick={() => go(item)}
                >
                  <Icon icon={item.kind === "run" ? "workflow" : "git-branch"} size={14} />
                  <span class="cmdk-label">{item.label}</span>
                  <span class="cmdk-sub mono">{item.sub}</span>
                </button>
              )}
            </For>
          </div>
        </div>
      </div>
    </Show>
  );
}
