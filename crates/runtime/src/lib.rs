//! `lumepeer-runtime` — the session runtime, with no window of its own
//! (ADR 0085 §1).
//!
//! Everything a Lumepeer node does that is not drawing: binding the endpoint,
//! the handshake, `SessionManager` and consent, the invite registry, the
//! address book, unattended admission, the audit log, capture and encode,
//! input, the clipboard, files, tunnels, terminals — all of it lived inside
//! `apps/desktop/src-tauri` until this crate, and none of it ever needed a
//! webview.
//!
//! **Why it is its own crate.** ADR 0085 puts a host on a machine where
//! nobody has signed in, in a process that must not link a browser engine and
//! must not be a second implementation of who may do what. Both of those
//! follow from the actor being a library: one implementation of authorization,
//! and a dependency list a privileged front end can be reviewed against.
//! `docs/gap-tasks/27-guest-core-library.md` wants the same thing from the
//! other side — a guest on a phone — and lands on this crate rather than
//! repeating the extraction.
//!
//! **What is not here, and why.** Windows. The [`view::ViewWindows`] trait is
//! the seam: this crate says *when* a window should exist and what is in it,
//! and a front end says *how* one is made. The Tauri implementation of that
//! trait, the host bar, the tray and the IPC surface of §13 all stay in
//! `apps/desktop/src-tauri`, which is the only place that knows what a window
//! is.
//!
//! The authorization rule is unchanged by the move and is worth restating
//! where the code now lives: `lumepeer-core` is the only thing that
//! authorizes. This crate owns the `SessionManager` and asks it; it never
//! decides in its place, and neither a front end nor a guest can widen a grant
//! (§2.1, §2.3).

#![forbid(unsafe_code)]

pub mod address_book_store;
pub mod audit_store;
pub mod clipboard_os;
pub mod config;
pub mod connection_history;
pub mod disk;
pub mod invite_store;
pub mod net_errors;
pub mod network;
pub mod recorder;
pub mod remembered_password;
pub mod session_agent;
pub mod system_power;
pub mod unattended_store;
pub mod view;
