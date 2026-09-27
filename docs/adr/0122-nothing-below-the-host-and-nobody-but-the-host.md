# ADR 0122 — Nothing below the host, and nobody but the host

Status: accepted
Date: 2026-09-27

A security review of the whole tree, asked for as "nobody may get into the
middle of a connection, and nobody may use Lumepeer to get in anywhere
quietly". The network half held up: iroh's TLS binds every connection to the
endpoint key, the obfuscated transport pins the host certificate, framing is
bounded before it allocates, and every grant is re-read in `lumepeer-core` at
the moment of use. What did not hold up is below it — the machine the host
runs on — plus a handful of places where a check existed by accident rather
than on purpose. One change for each. Amends
[ADR 0043](0043-one-privileged-helper-with-one-capability.md),
[ADR 0049](0049-the-helper-service-gets-a-second-capability.md),
[ADR 0053](0053-invite-ticket-carries-a-stun-address-and-pinned-cert-fingerprint.md),
[ADR 0057](0057-lumepeer-takes-full-local-control.md),
[ADR 0085](0085-the-host-can-be-a-service-and-the-screen-belongs-to-an-agent.md) and
[ADR 0114](0114-the-desktop-is-injected-by-a-system-worker.md).

## 1. The helper service answers elevated callers only

**Found.** `LumepeerHelper` runs as `LocalSystem`, and its pipe's access list
admitted `IU` — every interactive user. ADR 0043 chose that when the only
operation was a Ctrl+Alt+Del. Since ADR 0057 and ADR 0114 the same pipe also
injects input into the secure desktop and, through a `LocalSystem` worker,
into the ordinary one. The only other check is that the caller sits in the
console session, which any program the signed-in user runs does. So any
unelevated program could raise a UAC prompt for itself and ask the service to
press "Yes" on it, or type into an elevated window: a UAC bypass with this
service as the tool. The secure-desktop frame mapping had the same `IU` entry
for reading, which handed the UAC prompt and the lock screen to any
unelevated reader.

**Decision.** Both lists are `LocalSystem` and administrators
(`D:(A;;GA;;;SY)(A;;GA;;;BA)`). `BA` is only an enabled group in an elevated
token, so nothing below high integrity gets in. The one legitimate caller is
the client, which ADR 0057 already made always-elevated, so it loses nothing.
An unelevated development run loses the service and falls back to in-process
delivery, as it does on a machine with no service.

## 2. `%ProgramData%\Lumepeer` belongs to administrators

**Found.** Both services write there as `LocalSystem`: logs, and the host's
keystore, identity, address book and invite. `%ProgramData%` lets every user
create a folder and own it, and `log.rs` created `Lumepeer\logs` with
`create_dir_all`, inheriting "users may create files". `machine_store.rs`
re-applied its list to a `host` folder that already existed — but an owner
can rewrite a list at any time, so a user who made the folder before the
service first ran kept it, along with whatever keystore secret, password hash
or trusted device they had put in it. A junction in place of either folder
would have sent `LocalSystem`'s writes wherever it pointed.

**Decision.** `program_data.rs` secures each directory through a handle opened
without following reparse points: a reparse point is refused, the owner is
read from the directory itself, and the owner and a protected list are set
through the same handle. The root and the log folder are owned by
administrators, writable by `LocalSystem` and administrators, readable by
users (the logs are for whoever debugs the machine). A folder a user made in
their place is taken over, because nothing in it is read as a decision. The
host's store is **refused** if a user owns it — the host does not start, and
the log says to delete the folder — because its contents decide who gets in.
A log file this service did not write is removed before the first line.

## 3. A guest dials only the host that signed the ticket

**Found.** The host checks its own signature on a ticket; the guest never
checked it at all. The obfuscated transport pins the certificate whose
fingerprint the ticket carries, so a code edited on its way to the guest
could keep the host's identity and swap the fingerprint and address, and the
guest's first transport would connect to whoever held that certificate.

**Decision.** `InviteTicket::verify_issuer` checks the signature against the
key of the endpoint the ticket names, and `spawn_dial_as` refuses a ticket
that fails it before anything is dialed. Two checks follow the handshake: the
obfuscated transport refuses a pinned certificate whose key is not the named
host's, and `on_dialed` refuses any connection whose authenticated peer is
not the host the invite names. The TTL stays the host's to judge.

## 4. Each message comes from one end of the session

**Found.** `on_inbound` served both roles with one `match`. A guest could send
`ConsentGrant`, `UnattendedChallenge` or `RecordAck` to its host, and a host a
`DirListRequest` or `TerminalOpenRequest` to its guest. Every handler that
mattered refused on its own — mostly because the state it read belonged to
the other role — but that was a property of each handler, not of the
protocol. Chat was accepted from a guest the host had not admitted yet, so
anyone holding an invite code could put text in front of the person deciding
whether to admit them.

**Decision.** `MessageKind::direction` places every message once, exhaustively:
host to guest, guest to host, or either. Each connection records whether this
node dialed it, and a message from the wrong end is dropped before any
handler. Chat is accepted only within a granted session: on a host, from an
active session; on a guest, from a host it has a view of.

## 5. A one-time code is used once, and a lockout grows

**Found.** A TOTP code stayed valid for its whole ±1-step window after it had
let somebody in. The lockout was five attempts per five minutes, forever:
well over a thousand guesses a day against the device password of a trusted
device whose key had been taken.

**Decision.** The step of the last accepted code is remembered, and that code
and every earlier one are refused (RFC 6238 §5.2). Codes are compared without
an early exit. Every lockout after the first without a success in between is
twice as long, up to a day; a success resets the scale.

## 6. Smaller things

- The release main-window capability no longer lists `http://localhost:*` as
  a remote origin. Tauri already treats the dev server as local; the entry
  only meant that any localhost page the window was ever navigated to could
  set a device password. The pilot build keeps it.
- A release build no longer reads `config/*.toml` relative to the working
  directory. The elevated client started from a download folder would
  otherwise take its relay and log directory from whatever sat there.
- A signalling relay may send at most 64 KiB per WebSocket message, not the
  library's 64 MiB.

## What this does not change

- The per-user stores of the desktop client (Credential Manager,
  `%APPDATA%`) are still writable by any program the same user runs. A
  program already running as the user can plant a device password and a
  trusted device there. Moving the host's policy into an administrator-owned
  store, as the session-0 host already does, is the fix, and a larger one.
- Knock and host records on the Nostr relays are sealed with the invite key
  but not signed, so anybody holding an invite can redirect another guest's
  obfuscated dial to a wrong address. The pinned certificate turns that into a
  failed attempt, never a session with the wrong machine, and the dial falls
  back to iroh.
- The unattended password still travels to the host inside TLS. A guest that
  types it into a code an attacker handed it gives it to the attacker's
  machine; a PAKE would not.
- Anybody who knows a host's identity can hold its eight unauthenticated
  handshake slots open.

## Consequences

- An unelevated process can no longer reach `LumepeerHelper` at all,
  including the service's own endpoint test when run unelevated, which now
  skips.
- A machine where an ordinary user created `%ProgramData%\Lumepeer\host`
  refuses to host until an administrator deletes that folder.
- A ticket whose signature does not match the host it names fails at once
  with `InvalidTicket`, without dialing.
