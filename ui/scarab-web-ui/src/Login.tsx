// The sign-in screen (ADR-0049).
//
// Rendered by Layout in place of the entire app shell whenever there is no
// session, so it covers every route without touching the router: an expired
// session on a deep link lands here rather than on a page full of failed
// loads.
//
// The button is a plain <a>, not a router link, on purpose. `/v1/auth/login`
// is a SERVER route that 302s to the identity provider — client-side routing
// would try to match it against the SPA's routes and render nothing.
//
// With ONE provider the wording stays provider-agnostic: the provider is
// whatever the operator configured (GitHub, Forgejo, Dex, Keycloak — ADR-0049
// keeps identity forge-agnostic), so the button says where you are going, not
// who is at the other end. With SEVERAL (ADR-0069) the person has to choose,
// so each gets a button naming it — the default one first.
import { For, createResource } from "solid-js";
import emblemGold from "./assets/brand/scarab-emblem-dark.svg";
import { listAuthProviders } from "./api/client";
import { loginChoices } from "./identities";

export default function Login() {
  const [providers] = createResource(listAuthProviders);
  const choices = () => loginChoices(providers() ?? []);
  return (
    <div class="login">
      <div class="login-card">
        <img
          class="login-emblem"
          src={emblemGold}
          alt=""
          width={56}
          height={50}
        />
        <h1 class="login-title">Scarab</h1>
        <p class="login-sub">
          A run is durable state. Sign in to see them.
        </p>
        <div class="login-choices">
          <For each={choices()}>
            {(c) => (
              <a
                class={`btn ${c.primary ? "btn-primary" : "btn-ghost"} login-btn`}
                href={c.href}
                rel="nofollow"
              >
                {c.label}
              </a>
            )}
          </For>
        </div>
        <p class="login-note">
          You'll be redirected to your identity provider and back.
        </p>
      </div>
    </div>
  );
}
