# ADR 0044 — Remembering a guest-side device password

Status: accepted
Date: 2026-08-28

## Context

`invite-view.ts::credentialsPanel`'s own comment stated a rule: nothing in
that module keeps a copy of a password the user types into the credential
form (§8; ADR 0033). That rule protected against a copy accumulating in the
webview, the untrusted presentation layer (§2.3) — it never meant "this
password is retyped by hand on every visit forever," and the product
decision recorded as **D2** in `docs/bugs/DECISIONS.md` asks for exactly
that: a "remember this device's password" option, off by default, that
answers a known host's next credential challenge without a human retyping
it.

## Decisions

### 1. The copy lives in the OS keystore, in Rust, and never crosses back into the webview

`apps/desktop/src-tauri/src/remembered_password.rs` is a new,
guest-side-only store, modeled directly on the existing
`unattended_store.rs`: one keystore entry per host, named
`lumepeer.guest.password.<host_tag>`, where `host_tag` is the same stable,
install-salt-independent label `connection_history.rs` already keys its rows
on. `invite-view.ts`'s rule still holds exactly as written — no field of
that module ever holds a password — it is simply no longer the whole story:
a second copy now lives in the keystore, addressed by Rust, and is
substituted automatically the next time the same host's `UnattendedChallenge`
arrives. The webview never receives it back, in either direction: it only
ever sends a checkbox state (`remember: bool`) alongside the one submission
it already made.

### 2. A second, independent keystore handle, not a shared one

`ActorStores.keystore` is consumed whole by `UnattendedStore::new(keystore)`
inside `spawn_actor_with`, so nothing was left over for a second consumer.
Rather than change `UnattendedStore`'s ownership model to share a handle
(`Arc<dyn Keystore>`), `spawn_actor` opens the platform keystore a second
time for `ActorStores.remembered_password_keystore`. Every native backend
(`SecretServiceKeystore`, `AppleKeychainKeystore`,
`WindowsCredentialManagerKeystore`) already opens its own connection per
operation and holds no state between calls — their own doc comments say so —
so a second `open()` costs nothing beyond what the first one already pays,
and it keeps both stores as plain, independent owners of their
`Box<dyn Keystore>`.

### 3. The password is held in the actor for exactly one round trip, never longer

`Actor::pending_remember: Option<String>` exists only between
`on_unattended_submit` (set when `remember` was checked) and the outcome
that follows: a `ConsentGrant` writes it to the keystore and clears the
field, an `UnattendedReject` clears it without writing anything. Nothing
reads `pending_remember` back out for any other purpose, and it holds a
plaintext password no longer than the wire round trip already takes.

### 4. A remembered password is tried automatically, exactly once, and never for a second factor

On `MessageKind::UnattendedChallenge`, if the host did not also ask for a
one-time code, the actor checks the remembered-password store for that host
and, if it has an entry, submits it itself — the credentials modal never
opens, and the connect form shows only its ordinary "asking for a password"
status line (`ConnectStatusDto.credentials_auto`, surfaced to
`invite-view.ts` as `isCredentialsAuto()`).

The second factor is never saved and never auto-submitted, even when
`remember` was checked: `pending_remember` only ever holds the password. A
host requiring a one-time code always shows the modal — there is nothing to
answer with automatically, and that is the entire point of a second factor
(§8).

If the automatic attempt is refused, the stale entry is forgotten
(`RememberedPasswordStore::forget`) and the modal opens normally for the
user to answer by hand. It is not retried: a silent retry would spend the
same `CONSENT_RATE_PER_MINUTE` budget on a password already known to be
wrong, for no benefit.

### 5. Off by default, forgettable, and not itself a new grant

The checkbox defaults to unchecked, matching D2. Forgetting a remembered
password is a keystore operation the store already exposes
(`RememberedPasswordStore::forget`); a settings-window control to call it by
hand belongs to `docs/bugs/05-settings-window.md` (Пачка 4), which depends on
this batch finishing, and is out of scope here. Remembering a password
changes nothing about what a session may do once admitted — the four
independent grants of ADR 0029 and the role `unattended_set_role` configures
are exactly as unaffected as they are for a typed-by-hand admission.

## Consequences

- A device that reaches a host it already knows the password to signs in
  without the user typing anything, once `remember` was checked on a
  previous visit.
- A host's password change is discovered on the next connect: the stale
  remembered copy is refused once, forgotten, and the ordinary form takes
  over — no lockout risk from a silent retry loop.
- The keystore now holds one more class of secret per host that has ever
  been dialed with "remember" checked; `RememberedPasswordStore::forget` is
  the way to remove one, and a settings-window control for it is tracked
  separately (`docs/bugs/05-settings-window.md`).
