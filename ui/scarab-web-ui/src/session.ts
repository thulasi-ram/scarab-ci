// The signed-in principal, fetched once for the whole app.
//
// Several bits of chrome need it — the avatar, the Settings nav entry, the
// command palette — and they must agree. A module-level resource makes it one
// request and one answer instead of a fetch per consumer that could disagree
// mid-render.
import { createResource } from "solid-js";
import { getMe, type Me } from "./api/client";

const [me] = createResource<Me>(getMe);

export { me };
