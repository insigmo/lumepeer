# ADR 0033 — Unattended admission: the credential path, and keystore secret slots

Status: accepted
Date: 2026-08-27

## Context

`crates/core/src/unattended.rs` has shipped an Argon2id device password, an
RFC 6238 second factor and a brute-force lockout since the phase-7 catch-up.
Nothing called it: the only reference to the module anywhere in the repository
was `pub mod unattended;`.

Two things had to be settled before that could change.

**The rule text was wrong.** `CLAUDE.md` and `README.md` both listed "No
unattended access" among the non-negotiable rules, while the code implemented
exactly that. The module header cited an "ADR 0021" that was never written,
which made the contradiction look like an undocumented decision.

It was documented, under a different number. ADR 0023 §1 and §2 record the
Argon2id and TOTP decisions, dated 2026-08-22, taken from the project owner's
answers in `questions.md` ("Unattended-пароль: argon2 или PBKDF2? — решено
2026-08-22", "TOTP 2FA: самим или крейт? — решено 2026-08-22"). So unattended
access is a decided feature, the rule text is stale, and the dangling ADR 0021
reference is a numbering artifact. This ADR updates the rule to say what is
actually forbidden; the dangling numbers themselves belong to the ADR-debt
cleanup, which is tracked separately.

**Secrets had nowhere to live.** `CLAUDE.md` forbids secrets in `config/*.toml`,
and `crates/net::keystore` only knew how to store one thing: the endpoint
identity.

## Decisions

### 1. The rule becomes "no hidden capture", not "no unattended access"

What §2 actually protects is that a host machine never gives away its screen
without saying so on its own screen. An unattended host says so with a banner
that has no dismiss control (`unattended-settings.ts::unattendedIndicator`),
and the ordinary session indicator is unchanged. What stays forbidden, in the
same words as before: hidden capture, and bypassing OS permission prompts.

### 2. The keystore becomes a named-slot store

`Keystore` gains `load_secret`/`store_secret`/`delete_secret`, and the three
identity methods become provided methods over the slot named `IDENTITY_ENTRY`.
Every backend — Secret Service, Keychain, Windows Credential Manager,
`FileKeystore`, `MemoryKeystore` — implements storage once, for all slots.

An absent slot is `Ok(None)`, never an error: "nothing stored yet" is the
first-run state of every one of these entries.

`FileKeystore` keeps one file per slot. `IDENTITY_ENTRY` keeps the exact path
the store was constructed with, so an existing install still reads its
identity; every other entry becomes a sibling file whose name is the entry
folded down to `[a-z0-9-]`, which is what stops an entry name from addressing
anything outside the keystore directory. Encryption is unchanged, and
deliberately without the entry name as associated data: binding it would be
better, and would also make every already-stored identity undecryptable.

Three slots are added: the Argon2id PHC string, the TOTP secret, and the role.
The role is not secret. It lives here anyway so that it has one lifetime with
the credentials it applies to — clearing the password clears it in the same
pass, and a stale `FullControl` can never outlive the password it was chosen
for and attach itself to the next one.

### 3. `UnattendedAccess::admit` is the whole decision

The gate hands back a `Role`, not a boolean. The only way to obtain a `Role`
from this type is to have passed both factors, so no caller outside
`crates/core` can hold "the credentials were fine" as a value of its own and
pair it with a role of its choosing (§2.1). A caller that mishandles the `Err`
gets no role at all rather than a default one.

The role a successful admission is granted is host-configured and snapshotted
into the session at grant time, exactly like `set_control_policy`: changing it
never widens a session already running. It starts at `ViewOnly`.

### 4. A password policy, because leaving one out is also a policy

§8 fixes the lockout but no strength rule. Five attempts per 300 seconds is
only a meaningful defence if the secret is worth guessing at that rate, so
`UNATTENDED_PASSWORD_MIN_BYTES = 8` is enforced when the host *sets* a
password. It is a floor, not a recommendation. The error is raised only on
setting, never on presenting: telling a guest its guess was the wrong length
would narrow the search.

### 5. Protocol minor 4: three appended messages

`UnattendedChallenge { code_required }` (host to guest), `UnattendedAuth
{ password, code }` (guest to host) and `UnattendedReject(UnattendedRejection)`
(host to guest), appended after `SasAck`, behind the `Hello` feature string
`FEATURE_UNATTENDED`. A host that does not see the string falls back to the
ordinary consent path of §8.1 — which asks a human, the safe direction.

A success is **not** a new message. It is the ordinary `ConsentGrant`, so the
guest joins the admission path at exactly the point a human's click would have
put it, and there is only one way into a session.

`UnattendedRejection` is coarse in one direction only: "no password presented"
and "wrong password" collapse together, as do the two code cases, because the
difference is useful only to somebody probing the gate. What the guest needs
in order to act — which factor to retype, and how long a lockout has left —
survives.

`code_required` does tell an unadmitted peer whether a second factor is on.
That is a UI necessity: a guest cannot supply a code it was not asked for. The
disclosure is bounded by who can reach the challenge at all — a peer holding a
valid invite that the host's own address book marks trusted (ADR 0034).

### 6. Credentials are bounded at the parse boundary

`UNATTENDED_PASSWORD_MAX_BYTES` (1024) and `UNATTENDED_CODE_MAX_BYTES` (8) are
checked in `MessageEnvelope::decode`, before anything downstream allocates.
Credentials arrive from a peer that has not been admitted yet, which makes
them the least trusted payload the control channel carries. The code bound
leaves headroom over the six digits `Totp::verify` insists on, so a longer
code fails verification with a coarse `BadCode` rather than tearing down the
connection as a malformed frame.

### 7. Retries stay on the connection; the lockout is the only bound

A refused guest keeps its connection and its place in the pending set, so a
mistyped code does not mean redialing. There is no second counter anywhere:
`admit` owns the lockout, it counts on monotonic `Instant`, and adding a
per-connection attempt limit would only make the shared budget harder to
reason about.

## Consequences

- `PROTOCOL_MINOR` is 4 (5 after ADR 0032's `FileTransferStart`). Golden
  vectors for all three messages plus an over-limit code are frozen in
  `tests/interop/golden_vectors.txt`; earlier vectors are untouched.
- The lockout is in memory, so a restart clears it. Unchanged from the
  original design and acceptable for the same reason: every guess still costs
  a full Argon2id evaluation.
- Keystore writes now happen on the actor loop, on rare user-initiated
  settings changes. A Secret Service round trip there is a hazard ADR 0027
  would normally rule out; it is accepted because it is bounded to a host
  clicking "turn on", not to anything a peer can trigger.
- The TOTP secret crosses into the webview exactly once, when the factor is
  turned on, because an authenticator app cannot be provisioned without it.
  Nothing keeps a copy: `unattended_status` has no field that could return it.
- There is no QR code. `04-unattended-access.md` says to reuse the invite QR
  generator; no such generator exists in this repository (`grep -ri qr` finds
  nothing), and adding a QR dependency is its own decision. The secret is
  shown as selectable base32 plus the full `otpauth://` URI, which is what a
  QR would have encoded.
