# ADR 0123 — The password stays home, and so does the policy

Status: accepted
Date: 2026-09-27

The five risks [ADR 0122](0122-nothing-below-the-host-and-nobody-but-the-host.md)
listed under "What this does not change", each closed. Amends
[ADR 0033](0033-unattended-admission-and-keystore-secret-slots.md) (how a
device password is checked),
[ADR 0046](0046-the-guest-side-of-clipboard-sync-goes-automatic.md) (which
hosts a guest's clipboard goes to),
[ADR 0116](0116-the-rendezvous-pushes-through-nostr-relays.md) (what a guest
takes off a relay) and ADR 0085 §1's store placement for the desktop client.

## 1. A guest proves the password instead of sending it

**Was.** `UnattendedAuth` carried the device password in plaintext inside
TLS. TLS protects it from the network, not from the machine at the other end:
a guest handed somebody else's invite code, told "this is your office PC",
typed its office PC's password into a machine that then had it.

**Now.** A guest that advertises `unattended-proof` is challenged with
`UnattendedProofChallenge`: the salt and Argon2id costs of the host's stored
hash, and the host's SPAKE2 message. The guest runs the same Argon2id over
the password — the result is exactly the key the host's PHC string holds —
and uses it as the password of SPAKE2 (Ed25519 group, the RustCrypto
`spake2` crate), with both endpoint identities as the SPAKE2 identities. It
answers `UnattendedProof`: its SPAKE2 message, an HMAC-SHA256 under the
agreed key over the session id (the confirmation), and the one-time code
XORed with a pad derived from that key. The host checks the confirmation in
constant time; the lockout and one-time-code rules of ADR 0122 apply
unchanged, and a wrong answer is followed by a fresh challenge, since an
exchange is single-use.

What a host learns that is not the one the password was set on: one guess
per answer, nothing to test further guesses against offline. An answer is
bound to the host identity the guest believed and to the session, so it is
useless anywhere else. Existing passwords work without being set again,
because the host needs only its stored hash.

The cost, stated: the stored hash is now enough to *answer* a challenge, not
only to start cracking. It lives where only administrators can read it (§5)
on a Windows client, and in the host service's machine store already.

**Compatibility.** A host keeps the plaintext exchange for guests too old to
advertise the proof. A new guest sends the plaintext password only to a host
it has already had a session with — the same endpoint key, so the same
machine — and to any other host that asks for it that way says
`UNATTENDED_HOST_OUTDATED` and keeps the credential form up, since the host
can still let it in through the dialog. Once a host has asked this guest for
a proof on a connection, the guest never falls back to the plaintext on it:
a retry typed before the host's next challenge arrives waits for it.

## 2. A guest that only watches keeps its clipboard

**Was.** A guest offered its clipboard to every host it had a view onto,
whatever the role, and let the host's `clipboard_write` decide on arrival.
A host that only let the guest watch still received every password copied
on the guest's machine for as long as the window was open; an honest host
dropped it, and a dishonest one did not have to.

**Now.** The guest decides what leaves. It sends its clipboard, and its
clipboard's file list, only to a host whose session allows it:
`clipboard_write` and `file_transfer` respectively. The role gives the
starting values (full control has both), and a host tells a guest that
advertises `session-grants` whenever either moves, in `SessionGrants` — right
behind the grant and on every switch. A view-only guest does not even read
its own clipboard. An older host sends nothing, so its view-only guests keep
their clipboard, which is the direction to be wrong in.

## 3. A host's answer on a relay is signed

**Was.** Knock and host records on the Nostr relays were sealed under the
invite and signed by nobody. Anybody holding the invite could tell a guest
where the host "was", with a time far in the future so that the true answer
never replaced it; the pinned certificate made that a dead obfuscated path
rather than a session with the wrong machine, but a dead path all the same.
And every knock made the host send ten packets to whatever address it named.

**Now.** A host's record on the relays carries the host's ed25519 signature
over the record and the invite id, appended inside the seal where a reader
built before it ignores it. A guest takes a host record off a relay only if
that signature verifies against the host it is dialing. The DHT's copy is
already signed by the same key through BEP 44 and is unchanged.

A host punches towards one address at most once per
`RENDEZVOUS_REPUNCH_SECS` and sends at most `RENDEZVOUS_PUNCHES_PER_MINUTE`
trains in all, whoever knocks. Knocks stay unsigned: the host cannot know its
guests in advance, so there is nobody to sign them for.

## 4. A stalled handshake does not lock anybody out

**Was.** Eight handshake slots, held until a handshake finished or timed out,
and a full set refused every newcomer. Anybody who knew the host's identity
could keep it unreachable by opening eight connections every ten seconds and
never saying `Hello`. The obfuscated transport awaited each QUIC handshake
inside its accept loop, so one stalled handshake held every guest behind it.

**Now.** Thirty-two slots, and a full set drops the handshake that has waited
longest instead of refusing the newest (`Handshakes`). A genuine guest is
through within a round trip or two, long before its slot becomes the oldest.
The obfuscated accept loop hands each handshake to a task of its own, bounded
by `INCOMING_ACCEPT_TIMEOUT_SECS` and by the same drop-the-oldest rule.

## 5. An elevated Windows client keeps its policy where only administrators write

**Was.** The endpoint identity, the device password, its role and second
factor, and the passwords remembered for other hosts lived in the Credential
Manager; the address book with its trusted devices, the invite, the history
and the audit log in `%APPDATA%` and `%LOCALAPPDATA%`. Any program the same
account runs can write all of them, elevated or not. So a program with no
rights could set a device password, trust a device of its own, and then drive
this always-elevated client from the network — a quiet way in, and, through
ADR 0057's elevated input, a way up.

**Now.** On Windows, a run whose token is elevated keeps all of them in
`%ProgramData%\Lumepeer\users\<SID>`: owned by administrators, readable and
writable by `LocalSystem` and administrators only, the keystore an encrypted
file keyed by a secret beside it (`program_data::user_directory`,
`placement.rs`). A folder an ordinary user created under that name before the
tree was secured is passed over for `<SID>.1`, and so on, never used. The
first such run moves the profile's entries and files there, deletes them from
the profile, and never looks at the profile again, so what is planted there
afterwards is ignored.

An elevated run that cannot establish the directory at all runs with
unattended access off and no device trusted, on the profile's other stores,
and says so in its log. An unelevated development run, `LUMEPEER_KEYSTORE=file`
for the end-to-end rigs, Linux and macOS keep the profile: there is no account
boundary below those processes to put anything behind.

## What still does not change

- A knock can be written by anybody holding the invite; §3 bounds what it
  can make the host do, it does not make knocks authentic.
- On Linux and macOS a program running as the same user can still rewrite
  the user's stores. The session there has no elevated half to protect them.
- A downgrade to a build before this one, on Windows, finds no identity in
  the profile and mints a new one.

## Consequences

- Three new messages (`SessionGrants`, `UnattendedProofChallenge`,
  `UnattendedProof`) and two feature strings (`session-grants`,
  `unattended-proof`). All are sent only to a peer that advertised the
  string, so `PROTOCOL_MINOR` did not move; golden vectors for them belong
  with the next minor bump.
- New dependencies: `spake2` 0.4 and `sha2` 0.11 in `lumepeer-core`; everything
  `spake2` needs was already in the lockfile.
- One new failure code on the credential form, `UNATTENDED_HOST_OUTDATED`.
