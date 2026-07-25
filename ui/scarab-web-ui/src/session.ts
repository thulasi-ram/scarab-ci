// The signed-in principal, fetched once for the whole app (ADR-0049).
//
// Several bits of chrome need it — the avatar, the Settings nav entry, the
// command palette — and they must agree. A module-level resource makes it one
// request and one answer instead of a fetch per consumer that could disagree
// mid-render.
import { createResource } from "solid-js";
import { getMe, type Me } from "./api/client";

const [me] = createResource<Me>(getMe);

export { me };

/**
 * May the current principal administer org-level settings (ADR-0060)?
 *
 * Drives whether the Settings nav entry renders at all. Defaults to `false`
 * while loading or on error — chrome that appears and then vanishes is worse
 * than chrome that arrives a beat late. This is presentation only: the server
 * enforces `Administer` on every request regardless.
 */
export const canAdminister = () => me()?.can_administer ?? false;
