//! Keeping a session agent alive, and knowing when there is not one
//! (ADR 0085 §3).
//!
//! The privileged host has no screen. What it has is this loop, which asks the
//! machine one question on a tick — *is somebody signed in at the console* —
//! and turns the answer into the only three things a host can do about it:
//! launch an agent, serve through the one that attached, and notice when it is
//! gone.
//!
//! **The order is the whole design, and it is not this module's to choose.**
//! [`SessionScreen`] decides what to say and in what sequence; this module
//! carries it out. That split is why ADR 0085 §3's property — the indicator is
//! up before a single frame can leave — is provable on a machine with no
//! service installed on it at all, and why a bug in the supervision loop
//! cannot reorder it.
//!
//! **Three threads, and each of them is blocked on something different.** The
//! Win32 surface underneath has no way to wait on a pipe and a process at
//! once, so rather than pretend otherwise:
//!
//! - the *supervisor* blocks in `ConnectNamedPipe` and then in `recv`,
//! - the *writer* blocks on the command queue the actor feeds,
//! - the *watchdog* polls the agent process and breaks the other two out when
//!   it dies.
//!
//! The watchdog is the one that is easy to leave out and expensive to be
//! without: an agent that is launched and then dies before it connects would
//! otherwise leave the supervisor inside a wait no flag can interrupt, and the
//! machine would have no screen again until the service was restarted.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use lumepeer_runtime::session_agent::SessionScreen;
use lumepeer_service::agent_channel::{AgentCommands, AgentLink};
use lumepeer_service::agent_launch::{SessionAgent, console_session, user_sid};
use lumepeer_service::agent_protocol::AgentCommand;

use crate::view::AgentScreen;

/// How often the supervisor re-asks the machine who is signed in, and how
/// often the watchdog re-asks whether the agent is still there.
///
/// Half a second is the same order as the capture throttle the secure-desktop
/// latch already lives with: fast enough that a sign-out is noticed before a
/// guest could take it for a still picture, slow enough that asking the
/// session manager twice a second is not a cost anybody can measure.
pub const SUPERVISION_TICK: Duration = Duration::from_millis(500);

/// How long to wait before launching another agent after one failed to start.
///
/// Longer than the tick on purpose. A session whose agent cannot start — a
/// missing executable, a policy that refuses the launch — would otherwise be a
/// process created twice a second forever, which is a worse failure than the
/// missing picture it is trying to fix.
const RELAUNCH_BACKOFF: Duration = Duration::from_secs(5);

/// Which monitor the host asks the agent for.
///
/// Zero, and not settable from here. The index means something only inside the
/// agent's own session — it is an offset into what *that* desktop can see — so
/// a host choosing between monitors would be naming a thing it cannot
/// enumerate. Carrying a guest's monitor choice through to a service host is
/// listed in ADR 0087 as not done.
const MONITOR: u32 = 0;

/// Runs the supervision loop until `stopping` is set.
///
/// Every failure inside — no session, no agent, a channel that broke — is a
/// state to report and retry, not a reason to stop supervising a machine
/// people keep signing in and out of.
pub fn supervise(screen: &Arc<AgentScreen>, stopping: &AtomicBool) {
    while !stopping.load(Ordering::SeqCst) {
        let Some(session) = console_session() else {
            // Nobody is signed in. An ordinary state, not a fault: it is what
            // a machine at its logon screen looks like, and ADR 0085 §3a says
            // a guest admitted into it gets the honest "no picture" rather
            // than a frozen frame.
            screen.with_screen(SessionScreen::no_interactive_session);
            std::thread::sleep(SUPERVISION_TICK);
            continue;
        };

        if matches!(
            serve_one_session(screen, session, stopping),
            Served::CouldNotStart
        ) {
            std::thread::sleep(RELAUNCH_BACKOFF);
        }
    }
    // Leaving with the screen saying there is an agent would outlive this loop
    // by exactly as long as the process does, and anything still reading it
    // would be reading a promise nobody is keeping.
    screen.attach_commands(None);
    screen.with_screen(SessionScreen::agent_gone);
}

/// What one turn of [`supervise`] came to.
enum Served {
    /// An attachment happened and is over, or the loop is stopping. Go round
    /// again immediately: somebody may have signed straight back in.
    Ended,
    /// No agent could be launched or attached at all. Wait before trying
    /// again.
    CouldNotStart,
}

/// Launches an agent into `session` and serves through it until it goes away.
fn serve_one_session(screen: &Arc<AgentScreen>, session: u32, stopping: &AtomicBool) -> Served {
    // The SID before the process, because the channel's access list is built
    // from it and a channel that cannot name the user it admits is one this
    // host must not create. Asking here means that failure costs no process.
    let Some(sid) = user_sid(session) else {
        tracing::warn!(
            session,
            "cannot read the signed-in user's SID; no agent this round"
        );
        screen.with_screen(SessionScreen::no_interactive_session);
        return Served::CouldNotStart;
    };

    let Some(agent) = SessionAgent::start(session) else {
        // `SessionAgent::start` has already said which of the several reasons
        // it was.
        screen.with_screen(SessionScreen::agent_gone);
        return Served::CouldNotStart;
    };
    let pid = agent.pid();
    tracing::info!(session, pid, "session agent launched");
    screen.with_screen(|screen| screen.agent_launched(session));

    // Set for as long as this attachment should live. The watchdog clears it,
    // and the accept loop reads it — which is what lets a dead agent end a
    // wait no flag can interrupt.
    let attached = AtomicBool::new(true);
    // `SessionAgent` is `Send` but not `Sync` — it owns raw process handles —
    // and the watchdog needs to ask it two questions from another thread. A
    // mutex is the whole of what that needs: both questions are a single Win32
    // call, nothing is held across a wait, and the supervisor takes it back by
    // value at the end to stop the agent.
    let agent = std::sync::Mutex::new(agent);

    // A scope rather than `spawn`: the watchdog borrows `agent`, `attached`
    // and `stopping`, and a scope is how "this thread is joined before those
    // borrows end" is said to the compiler rather than asserted in a comment.
    let outcome = std::thread::scope(|threads| {
        let watchdog = threads.spawn(|| watch(&agent, &attached, stopping));
        let outcome = attach_and_serve(screen, pid, &sid, &attached, stopping);
        // Ends the watchdog whichever way the attachment finished.
        attached.store(false, Ordering::SeqCst);
        let _ = watchdog.join();
        outcome
    });

    screen.attach_commands(None);
    screen.with_screen(SessionScreen::agent_gone);
    // `stop` rather than letting the handle drop: dropping stops *watching* an
    // agent, which would leave a process serving a desktop this host no longer
    // believes in.
    match agent.into_inner() {
        Ok(agent) => agent.stop(),
        // A poisoned mutex means the watchdog panicked holding it. The agent
        // is still a live process, so it is still stopped — a panic in a
        // liveness poll is no reason to leave somebody's session with an
        // indicator on it.
        Err(poisoned) => poisoned.into_inner().stop(),
    }
    outcome
}

/// Waits for the agent to connect, then pumps events until the channel ends.
fn attach_and_serve(
    screen: &Arc<AgentScreen>,
    pid: u32,
    sid: &str,
    attached: &AtomicBool,
    stopping: &AtomicBool,
) -> Served {
    let keep_waiting = || attached.load(Ordering::SeqCst) && !stopping.load(Ordering::SeqCst);
    let Some(mut link) = AgentLink::accept_from_while(pid, sid, &keep_waiting) else {
        tracing::warn!(pid, "the session agent never connected");
        return Served::CouldNotStart;
    };
    let Some(commands) = link.commands() else {
        return Served::CouldNotStart;
    };

    // The writer thread owns the only handle that can send, so nothing else
    // can interleave half a message into the pipe.
    let (tx, rx) = mpsc::channel::<AgentCommand>();
    let writer = std::thread::spawn(move || write_commands(commands, &rx));
    screen.attach_commands(Some(tx.clone()));

    while let Some(event) = link.recv() {
        // The state machine decides; this loop only carries out. Commands come
        // back in the order they must be sent, and sending them in that order
        // is the whole of ADR 0085 §3b.
        let commands = screen.with_screen(|screen| screen.on_event(event, MONITOR));
        let delivered = commands.into_iter().all(|command| tx.send(command).is_ok());
        if !delivered {
            // The writer has gone, which means the pipe has. Reading on would
            // be waiting for an agent that cannot be told anything.
            break;
        }
        if stopping.load(Ordering::SeqCst) {
            // Ask rather than kill: `Shutdown` is what lets an agent drop its
            // indicator and stop capture on its way out, instead of leaving a
            // banner on a screen until the session ends.
            let _ = tx.send(AgentCommand::Shutdown);
            break;
        }
    }

    screen.attach_commands(None);
    // Dropping every sender is what ends the writer thread.
    drop(tx);
    let _ = writer.join();
    Served::Ended
}

/// Writes queued commands until the queue or the pipe ends.
fn write_commands(mut commands: AgentCommands, queue: &mpsc::Receiver<AgentCommand>) {
    while let Ok(command) = queue.recv() {
        if !commands.send(command) {
            tracing::warn!("the session agent channel stopped accepting commands");
            return;
        }
    }
}

/// Watches the agent process and ends the attachment when it goes away.
///
/// The connect at the end is the point. `ConnectNamedPipe` and a blocking
/// `read_exact` are both waits no flag interrupts, and the established way to
/// break one in this codebase is to connect to the pipe — `windows_service.rs`
/// does exactly this to its own. A connection from this process is not the
/// agent, so the accept loop turns it away, and on the next round it asks
/// `keep_waiting` and gives up.
fn watch(agent: &std::sync::Mutex<SessionAgent>, attached: &AtomicBool, stopping: &AtomicBool) {
    /// One look at the agent: still running, and still the console session's.
    fn still_serving(agent: &std::sync::Mutex<SessionAgent>) -> bool {
        let agent = agent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !agent.is_alive() {
            tracing::info!(pid = agent.pid(), "the session agent exited");
            return false;
        }
        // A fast user switch leaves the agent alive in a session that is no
        // longer the console one. Its pixels belong to somebody who is no
        // longer at the machine, so the attachment ends rather than carrying
        // on showing them.
        if !agent.serves_the_console() {
            tracing::info!(
                pid = agent.pid(),
                "the session agent no longer serves the console session"
            );
            return false;
        }
        true
    }

    while attached.load(Ordering::SeqCst) && !stopping.load(Ordering::SeqCst) {
        if !still_serving(agent) {
            break;
        }
        std::thread::sleep(SUPERVISION_TICK);
    }
    attached.store(false, Ordering::SeqCst);
    let _ = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(lumepeer_service::agent_protocol::AGENT_ENDPOINT);
}
