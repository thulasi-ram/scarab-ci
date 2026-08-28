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
// Deliberately provider-agnostic wording: the provider is whatever the operator
// configured (GitHub, Forgejo, Dex, Keycloak — ADR-0049 keeps identity
// forge-agnostic), and the server never tells the browser which one it is. So
// the button says where you are going, not who is at the other end.
import emblemGold from "./assets/brand/scarab-emblem-dark.svg";

export default function Login() {
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
        <a class="btn btn-primary login-btn" href="/v1/auth/login" rel="nofollow">
          Sign in
        </a>
        <p class="login-note">
          You'll be redirected to your identity provider and back.
        </p>
      </div>
    </div>
  );
}
