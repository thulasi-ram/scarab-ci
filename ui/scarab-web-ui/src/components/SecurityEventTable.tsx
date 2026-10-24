// A table of security events (ADR-0073 §7) — the OrgSecurityEventLog on
// Settings → Security and your own events on Account → Security. Rows are
// shaped by `eventRow` (security.ts); this only lays them out.
import { For, Show } from "solid-js";
import type { SecurityEvent } from "../api/client";
import { absTime, relTime } from "../fmt";
import { eventRow } from "../security";

export default function SecurityEventTable(props: {
  events: SecurityEvent[];
  /** Hide the "who" column (your own events are all about you). */
  hideWho?: boolean;
  empty?: string;
}) {
  return (
    <Show
      when={props.events.length > 0}
      fallback={<p class="empty">{props.empty ?? "No security events yet."}</p>}
    >
      <table class="table sec-table">
        <thead>
          <tr>
            <th>When</th>
            <th>Event</th>
            <Show when={!props.hideWho}>
              <th>Who</th>
            </Show>
            <th>Via</th>
            <th>What</th>
            <th>Note</th>
          </tr>
        </thead>
        <tbody>
          <For each={props.events.map(eventRow)}>
            {(r) => (
              <tr>
                <td class="mono" title={absTime(r.at)}>
                  {relTime(r.at)}
                </td>
                <td>
                  <span class={`sec-kind sec-${r.tone}`} title={r.kind}>
                    {r.label}
                  </span>
                </td>
                <Show when={!props.hideWho}>
                  <td class="mono">{r.who}</td>
                </Show>
                <td>{r.via}</td>
                <td class="mono">{r.what}</td>
                <td class="subtle">{r.note}</td>
              </tr>
            )}
          </For>
        </tbody>
      </table>
    </Show>
  );
}
