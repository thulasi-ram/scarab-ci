// The forge connections that serve no org yet, fetched once for the whole app.
//
// Two places read them: the New organization page lists them, and the org
// dropdown and the command palette count them on the New organization entry.
// A module-level resource makes that one request per page load instead of
// one per consumer (or per keystroke in the palette). Only a global admin is
// sent them, so for anyone else the request is not made.
import { createResource } from "solid-js";
import { listConnections } from "./api/client";
import { unassignedConnections } from "./connections";
import { me } from "./session";

const [waitingConnections, { refetch: refetchWaiting }] = createResource(
  () => me()?.global_admin === true,
  () => listConnections().then(unassignedConnections),
);

/** How many connections wait for an org; 0 while loading, on an error, or
 *  for a caller who is not a global admin. */
export function waitingCount(): number {
  if (waitingConnections.error) return 0;
  return waitingConnections.latest?.length ?? 0;
}

// Refetched after a global admin assigns or removes one.
export { waitingConnections, refetchWaiting };
