# 0079. Scarab-native authority moves only on Scarab-level proof

- **Status:** Accepted
- **Date:** 2026-11-07
- **Deciders:** thulasi.ram (architect)
- **Supersedes in part:** [0069](0069-identity-links.md) §6 (a self-service merge on
  the OAuth round-trip alone) and the 2026-11-07 owner decision on git-bug def16bd
  (a merge moves the folded Principal's passkeys).
- **Builds on:** [0049](0049-identity-and-access.md) (native RBAC, forge-permission
  import), [0069](0069-identity-links.md) (Principals, identities, merge),
  [0073](0073-step-up-confirmation.md) (passkey step-up)
- **Tickets:** git-bug bbb5204 (the takeover), 7905f89 (the web UI for the merge)

## Context

Three decisions, each sound when it was made, combined into a takeover.

1. **ADR-0069 §6 (2026-10-16).** When a user links a sign-in identity that another
   Principal P holds as its only identity, the OAuth round-trip "proved the caller
   controls this identity", so P is merged into the caller. The merge moves P's
   native role bindings, rewrites every approver entry naming P, re-owns P's API
   tokens and deletes P's sessions. At the time, a forge or OIDC login was the
   strongest proof Scarab had.
2. **ADR-0073 (2026-10-24).** Protected operations need a passkey of the acting
   Principal, because a session or a forge login alone is not enough. Once a
   Principal has a passkey, a fresh login cannot enroll a new first passkey, so a
   stolen forge login does not give step-up power. ADR-0069's merge rule was not
   revisited, and the forge login became the weaker of the two proofs.
3. **def16bd (2026-11-07).** A merge moves the folded Principal's passkeys to the
   surviving one, on the reasoning that a merge asserts the two ids are the same
   person.

The attack (red team RT-4, safety A2): Mallory has her own Principal with her own
passkey and controls Ada's GitHub login, which is the only sign-in of Ada's Principal.
Signed in as herself, Mallory links GitHub, signs in as Ada, and Ada's Principal is
folded into hers. Ada's native roles and approver entries now belong to Mallory, who
confirms protected operations with her own passkey. Ada's GitHub sign-in now leads to
Mallory's Principal, so Ada is locked out.

Without the merge, Mallory could sign in as Ada and hold Ada's roles, but every
protected operation would ask for Ada's passkey, which she does not have. The merge
adds the power to act. Ada's passkeys are not what Mallory gains; she cannot use them
without Ada's device. The roles and approver entries are.

Two kinds of authority sit on a Principal, and the merge treated them alike:

| Kind | Source | Who should hold it |
|---|---|---|
| Forge-imported roles (import rows, ADR-0049) | synced from the forge's membership | whoever holds that forge identity; the next sync gives them to that holder anyway |
| Scarab-native authority: native role grants, approver entries, API tokens, passkeys | granted or enrolled in Scarab | that Principal only |

## Decision

### 1. The rule

An OAuth or OIDC round-trip proves control of a sign-in identity. It may grant what
comes from that provider, such as forge-imported roles. It never moves Scarab-native
authority away from the Principal that holds it.

Moving Scarab-native authority from one Principal to another needs Scarab-level
proof from the Principal that loses it: a passkey assertion of that Principal, or an
action by an Owner who is accountable for it.

Any new flow that moves, merges, transfers or re-owns a Principal's grants, approver
entries, tokens or credentials is checked against this rule before it ships: what
proof does it accept, and whose?

### 2. Self-service merge (ADR-0069 §6, amended)

- **The folded Principal holds any passkey**, a flagged one included. The merge
  also needs an assertion from one of the folded Principal's passkeys, in the same
  flow as the link. The surviving Principal's proof is the window step-up the link
  already asks for. Without the assertion, the link changes nothing.
- **The folded Principal holds no passkey.** Unchanged: the OAuth round-trip
  is enough. Whoever controls that sign-in could already sign in as that Principal
  and enroll its first passkey (ADR-0073 §2), so the merge gives away nothing the
  login did not. This covers the common onboarding case: a Principal created by
  forge import that nobody has signed in as.
- **The folded Principal's passkeys are lost or all flagged.** An Owner resets
  them (the existing `passkey.reset`); the merge then proceeds as above. A flagged
  passkey counts as held: otherwise the merge would skip the rule that an Owner
  resets flagged passkeys before a fresh login can enroll a new one (ADR-0073 §2).

### 3. Passkeys are not moved

A passkey is registered for one Principal, under that Principal's WebAuthn user
handle. Moving it to another Principal makes it vouch for an account it was never
registered for. On a merge, the folded Principal's passkeys are used as proof (§2)
and then deleted, each recorded as a security event naming both Principals. The
person enrolls a passkey on the surviving Principal. The route
`POST /v1/me/identities/link-moving-passkeys` and the per-passkey `user_handle`
carried for moved passkeys (migration 0082) are no longer needed.

## Considered options

- **Never fold a Principal that holds passkeys.** Simpler code, but to merge, the
  person first removes the old Principal's passkeys, which asks for one of those
  passkeys anyway, or waits for an Owner. It reaches the same proof as §2 in more
  steps.
- **An Owner approves passkey-holding merges.** A person looks at each one, but
  the Owner does not prove the requester controls the folded Principal, an Owner
  is not always available, and an Owner merging their own Principals approves
  themselves.
- **Self-service merge only for a Principal nobody has used; every other merge by
  an admin.** It closes the hole without a second assertion, but a person who
  clicked the wrong sign-in button cannot fix it alone, and the admin merge
  ADR-0069 lists as out of scope would have to be built first.
- **Drop self-service merge.** The same cost as the previous option, plus the
  onboarding case needs an admin.
- **Stop moving passkeys, nothing else.** This alone does not close the hole: the
  roles and approver entries still move on the OAuth round-trip.
- **Leave it.** The takeover stays open.

## Consequences

- A merge of two Principals that both hold passkeys asks for two passkey
  assertions: one per Principal. A synced passkey manager answers both on one
  device.
- A merge where the folded Principal has no passkey asks for nothing new.
- A person who lost the folded Principal's passkey cannot merge without an Owner.
- After a merge, the person enrolls a passkey on the surviving Principal if they
  want the folded one's devices to work; the old entry stays in their password
  manager, pointing at a Principal that no longer signs in.
- The web UI merge (7905f89) is a two-assertion flow on the one link route.
- Forge-imported roles keep following the forge identity, as ADR-0049 intends.
