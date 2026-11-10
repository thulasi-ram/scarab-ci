// The org's security event log (git-bug fa23028), below the posture panel of
// Organization settings → Security. One server page at a time, newest first;
// filters by kind, actor, target text and time range, and a preset for the
// last 24 hours of refusals. The filters and the page's cursor live in the
// URL (`event-log.ts`), so a filtered view can be shared and Back returns to
// the newer page. The Security section reads no other search param, so these
// names are the log's alone.
import { For, Show, createEffect, createMemo, createResource, createSignal, on } from "solid-js";
import { useSearchParams } from "@solidjs/router";
import { listOrgSecurityEvents } from "../api/client";
import { Field, Input, Select } from "./Field";
import {
  emptyText,
  filterKey,
  filterSearch,
  fromLocalInput,
  isFiltered,
  rangeProblem,
  readView,
  refusalsLastDay,
  requestFor,
  toLocalInput,
  type EventLogFilter,
} from "../event-log";
import { EVENT_KINDS, eventKindLabel } from "../security";
import SecurityEventTable from "./SecurityEventTable";

export default function EventLog(props: { org: string }) {
  const [search, setSearch] = useSearchParams();
  const view = () => readView(search);

  // The form's drafts; the URL is the truth, and the drafts follow its
  // filters (Back, a pasted link) until someone edits them. Paging changes
  // only the cursor, so it leaves a draft not yet applied alone.
  const [actor, setActor] = createSignal("");
  const [target, setTarget] = createSignal("");
  const [from, setFrom] = createSignal("");
  const [to, setTo] = createSignal("");
  const filters = createMemo(() => filterKey(view()));
  createEffect(
    on(filters, () => {
      const v = view();
      setActor(v.actor);
      setTarget(v.target);
      setFrom(toLocalInput(v.from));
      setTo(toLocalInput(v.to));
    }),
  );

  const draft = (kind = view().kind): EventLogFilter => ({
    kind,
    actor: actor(),
    target: target(),
    from: fromLocalInput(from()),
    to: fromLocalInput(to()),
  });
  // A filter change replaces the history entry; paging pushes one.
  const apply = (f: EventLogFilter) => setSearch(filterSearch(f), { replace: true });
  const clear = () => apply({ kind: "", actor: "", target: "", from: "", to: "" });

  // A wrong time range sends nothing; the form says what is wrong.
  const [page] = createResource(
    () => {
      const q = requestFor(view());
      return q && { org: props.org, q };
    },
    (k) => listOrgSecurityEvents(k.org, k.q),
  );
  // The last page that loaded, kept on screen while the next one loads;
  // nothing once a load failed (reading a failed resource throws).
  const latest = () => (page.error ? undefined : page.latest);
  const problem = () => rangeProblem(view());

  return (
    <div class="panel">
      <div class="panel-h">
        <span>Event log</span>
      </div>
      <div class="secrets-body">
        <form
          class="secrets-actions sec-filter evlog-filter"
          onSubmit={(e) => {
            e.preventDefault();
            apply(draft());
          }}
        >
          <Field label="Kind">
            <Select value={view().kind} onValue={(v) => apply(draft(v))}>
              <option value="">All events</option>
              <For each={EVENT_KINDS}>
                {(k) => (
                  <option value={k}>{eventKindLabel(k)}</option>
                )}
              </For>
            </Select>
          </Field>
          <Field label="Actor">
            <Input mono placeholder="subject, e.g. j.lee" value={actor()} onValue={setActor} />
          </Field>
          <Field label="Target contains">
            <Input placeholder="Run id, project, secret" value={target()} onValue={setTarget} />
          </Field>
          <Field label="From">
            <Input type="datetime-local" value={from()} onValue={setFrom} />
          </Field>
          <Field label="To">
            <Input type="datetime-local" value={to()} onValue={setTo} />
          </Field>
          <button class="btn btn-primary btn-sm" type="submit">
            Apply
          </button>
          <button class="btn btn-ghost btn-sm" type="button" onClick={() => apply(refusalsLastDay(Date.now()))}>
            Refusals, last 24h
          </button>
          <Show when={isFiltered(view())}>
            <button class="btn btn-ghost btn-sm" type="button" onClick={clear}>
              Clear filters
            </button>
          </Show>
        </form>
        <p class="subtle">
          <small>
            Target matches the text in the What column: a Run id, a project or a secret name. It
            finds only events bound to this organization. Times are your local time.
          </small>
        </p>

        <Show when={problem()}>{(p) => <p class="error">{p()}</p>}</Show>
        <Show when={page.error}>
          <p class="error">
            {page.error instanceof Error ? page.error.message : "Could not load the event log."}
          </p>
        </Show>
        <Show when={page.loading && !latest()}>
          <p class="empty">loading…</p>
        </Show>
        <Show when={!problem() && latest()}>
          {(p) => (
            <SecurityEventTable events={p().events} empty={emptyText(view(), !!view().cursor)} />
          )}
        </Show>

        <Show when={view().cursor || latest()?.next_cursor}>
          <div class="secrets-actions">
            <Show when={view().cursor}>
              <button
                class="btn btn-ghost btn-sm"
                disabled={page.loading}
                onClick={() => setSearch({ cursor: "" })}
              >
                Newest events
              </button>
            </Show>
            <Show when={latest()?.next_cursor}>
              {(c) => (
                <button
                  class="btn btn-ghost btn-sm"
                  disabled={page.loading}
                  onClick={() => setSearch({ cursor: c() })}
                >
                  Older events
                </button>
              )}
            </Show>
          </div>
        </Show>
      </div>
    </div>
  );
}
