//! Keeping a session agent alive, and knowing when there is not one
//! (ADR 0085 §3, ADR 0088).
//!
//! The privileged host has no screen. What it has is this loop, which asks the
//! machine one question on a tick — *what is on the console right now* — and
//! turns the answer into the only things a host can do about it: serve the
//! signed-in user's desktop through their agent, serve the logon screen
//! through its worker, or say there is nothing to serve.
//!
//! **The order is the whole design, and it is not this module's to choose.**
//! [`SessionScreen`] decides what to say and in what sequence; this module
//! carries it out. That split is why ADR 0085 §3's property — the indicator is
//! up before a single frame can leave — and ADR 0088 §2's — no frame of a
//! session survives the switch away from it — are provable on a machine with
//! no service installed on it at all, and why a bug in the supervision loop
//! cannot reorder them.
//!
//! **Three threads, and each of them is blocked on something different.** The
//! Win32 surface underneath has no way to wait on a pipe and a process at
//! once, so rather than pretend otherwise:
//!
//! - the *supervisor* blocks in `ConnectNamedPipe` and then in `recv`,
//! - the *writer* blocks on the command queue the actor feeds,
//! - the *watchdog* polls the attached process and the session notifications,
//!   and breaks the other two out when either says the attachment is over.
//!
//! The watchdog is the one that is easy to leave out and expensive to be
//! without: an agent that is launched and then dies before it connects would
//! otherwise leave the supervisor inside a wait no flag can interrupt, and the
//! machine would have no screen again until the service was restarted.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use lumepeer_runtime::session_agent::{SessionAction, SessionScreen};
use lumepeer_service::agent_channel::{AgentCommands, AgentLink};
use lumepeer_service::agent_launch::{SessionAgent, console_session, user_sid};
use lumepeer_service::agent_protocol::AgentCommand;
use lumepeer_service::frame::Writer;
use lumepeer_service::logon_screen::LogonScreenWorker;

use crate::view::AgentScreen;

/// How often the supervisor re-asks the machine who is signed in, and how
/// often the watchdog re-asks whether the agent is still there.
///
/// Half a second is the same order as the capture throttle the secure-desktop
/// latch already lives with: fast enough that a sign-out is noticed before a
/// guest could take it for a still picture, slow enough that asking the
/// session manager twice a second is not a cost anybody can measure. Session
/// notifications (ADR 0088 §2) are read on the same tick, so a switch is acted
/// on within one of them even when the process check alone would miss it.
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
        let console = console_session();
        // Notifications that arrived between attachments. Nothing is attached
        // to end, but the lock they may carry decides what to serve next.
        screen.with_screen(|screen| {
            screen.console_session_is(console);
            fold_session_changes(screen);
        });

        let Some(session) = console else {
            // No console session at all. An ordinary state, not a fault, and
            // the machine's own answer — which is what makes it a confirmed
            // state that may end a transition (ADR 0088 §2).
            screen.with_screen(SessionScreen::no_interactive_session);
            std::thread::sleep(SUPERVISION_TICK);
            continue;
        };

        // Nobody signed in, or signed in and locked: either way the console
        // shows `Winlogon`, and the logon screen is what there is to serve.
        let locked = screen.with_screen(|screen| screen.locked());
        let served = match user_sid(session) {
            Some(sid) if !locked => serve_agent(screen, session, &sid, stopping),
            _ => serve_logon_screen(screen, session, stopping),
        };
        if matches!(served, Served::CouldNotStart) {
            std::thread::sleep(RELAUNCH_BACKOFF);
        }
    }
    // Leaving with the screen saying there is an agent would outlive this loop
    // by exactly as long as the process does, and anything still reading it
    // would be reading a promise nobody is keeping.
    screen.attach_commands(None);
    screen.with_screen(SessionScreen::agent_gone);
}

/// Folds every queued session change into `screen`, and answers whether any of
/// them ended the attachment.
///
/// A change about a session this host does not serve is logged and changes
/// nothing (ADR 0088 §4) — saying so is the whole of what "this session is not
/// served" means on a machine with no screen of its own to say it on.
fn fold_session_changes(screen: &mut SessionScreen) -> bool {
    let mut ended = false;
    for change in crate::service::take_session_changes() {
        match screen.session_changed(change) {
            SessionAction::Ignore => tracing::info!(
                ?change,
                "a session change outside the console session; that session is not served"
            ),
            SessionAction::KeepServing => {}
            SessionAction::EndAttachment => {
                tracing::info!(?change, "a session change ends the current attachment");
                ended = true;
            }
        }
    }
    ended
}

/// What one turn of [`supervise`] came to.
enum Served {
    /// An attachment happened and is over, or the loop is stopping. Go round
    /// again immediately: somebody may have signed straight back in.
    Ended,
    /// Nothing could be launched or attached at all. Wait before trying again.
    CouldNotStart,
}

/// A launched process this host serves a screen through.
///
/// The session agent and the logon-screen worker are different processes on
/// different desktops, run as different accounts; what the supervision loop
/// needs from either is these four questions, and nothing about which one it
/// is.
trait Attachment: Send {
    /// The process id the channel's far end is checked against.
    fn pid(&self) -> u32;
    /// Whether the process is still running.
    fn is_alive(&self) -> bool;
    /// Whether it is still on the session attached to the console.
    fn serves_the_console(&self) -> bool;
    /// Stops it, terminating it if it will not leave.
    fn stop(self);
}

impl Attachment for SessionAgent {
    fn pid(&self) -> u32 {
        Self::pid(self)
    }
    fn is_alive(&self) -> bool {
        Self::is_alive(self)
    }
    fn serves_the_console(&self) -> bool {
        Self::serves_the_console(self)
    }
    fn stop(self) {
        Self::stop(self);
    }
}

impl Attachment for LogonScreenWorker {
    fn pid(&self) -> u32 {
        Self::pid(self)
    }
    fn is_alive(&self) -> bool {
        Self::is_alive(self)
    }
    fn serves_the_console(&self) -> bool {
        Self::serves_the_console(self)
    }
    fn stop(self) {
        Self::stop(self);
    }
}

/// Launches an agent into `session` and serves through it until it goes away.
fn serve_agent(
    screen: &Arc<AgentScreen>,
    session: u32,
    sid: &str,
    stopping: &AtomicBool,
) -> Served {
    // The mapping before the process, and held by this side for the whole
    // attachment. The agent *opens* it and never creates one: a mapping the
    // agent created would be one whose access list the agent chose, and the
    // whole point of `create_for_session_agent` is that the list names that
    // one signed-in user and nobody else. Holding it here is also what keeps
    // it alive — an agent that died would otherwise take the mapping with it,
    // mid-read.
    let Some(mapping) = Writer::create_for_session_agent(sid) else {
        tracing::error!(
            session,
            "cannot publish a frame mapping for this session's agent"
        );
        screen.with_screen(SessionScreen::agent_gone);
        return Served::CouldNotStart;
    };

    let Some(agent) = SessionAgent::start(session) else {
        // `SessionAgent::start` has already said which of the several reasons
        // it was.
        screen.with_screen(SessionScreen::agent_gone);
        return Served::CouldNotStart;
    };
    tracing::info!(session, pid = agent.pid(), "session agent launched");
    screen.with_screen(|screen| screen.agent_launched(session));

    serve(screen, agent, &mapping, stopping, &|pid, keep_waiting| {
        AgentLink::accept_from_while(pid, sid, keep_waiting)
    })
}

/// Launches the logon-screen worker onto `session`'s `Winlogon` desktop and
/// serves through it until it goes away (ADR 0088 §1).
fn serve_logon_screen(screen: &Arc<AgentScreen>, session: u32, stopping: &AtomicBool) -> Served {
    // The mapping before the process, for the agent's reason above; its access
    // list admits no user, because there is none.
    let Some(mapping) = Writer::create_for_logon_screen() else {
        tracing::error!(
            session,
            "cannot publish a frame mapping for the logon screen"
        );
        screen.with_screen(SessionScreen::agent_gone);
        return Served::CouldNotStart;
    };

    let Some(worker) = LogonScreenWorker::start(session) else {
        screen.with_screen(SessionScreen::agent_gone);
        return Served::CouldNotStart;
    };
    tracing::info!(session, pid = worker.pid(), "logon-screen worker launched");
    screen.with_screen(|screen| screen.logon_screen_launched(session));

    serve(
        screen,
        worker,
        &mapping,
        stopping,
        &AgentLink::accept_from_system_while,
    )
}

/// How the channel's far end is accepted: by the launched process's pid, while
/// the closure says to keep waiting.
type Accept<'a> = dyn Fn(u32, &dyn Fn() -> bool) -> Option<AgentLink> + Sync + 'a;

/// Serves through one launched process until the attachment ends, then stops it
/// and leaves nothing of it behind.
fn serve<A: Attachment>(
    screen: &Arc<AgentScreen>,
    process: A,
    mapping: &Writer,
    stopping: &AtomicBool,
    accept: &Accept<'_>,
) -> Served {
    let pid = process.pid();
    // Set for as long as this attachment should live. The watchdog clears it,
    // and the accept loop reads it — which is what lets a dead process, or a
    // session change, end a wait no flag can interrupt.
    let attached = AtomicBool::new(true);
    // The launched value owns raw process handles and is `Send` but not
    // `Sync`, and the watchdog needs to ask it questions from another thread.
    // A mutex is the whole of what that needs: every question is a single
    // Win32 call, nothing is held across a wait, and the supervisor takes it
    // back by value at the end to stop the process.
    let process = std::sync::Mutex::new(process);

    // A scope rather than `spawn`: the watchdog borrows `process`, `attached`
    // and `stopping`, and a scope is how "this thread is joined before those
    // borrows end" is said to the compiler rather than asserted in a comment.
    let outcome = std::thread::scope(|threads| {
        let watchdog = threads.spawn(|| watch(screen, &process, &attached, stopping));
        let keep_waiting = || attached.load(Ordering::SeqCst) && !stopping.load(Ordering::SeqCst);
        let outcome = if let Some(link) = accept(pid, &keep_waiting) {
            pump(screen, link, &attached, stopping)
        } else {
            tracing::warn!(pid, "the launched process never connected");
            Served::CouldNotStart
        };
        // Ends the watchdog whichever way the attachment finished.
        attached.store(false, Ordering::SeqCst);
        let _ = watchdog.join();
        outcome
    });

    screen.attach_commands(None);
    // Only a state the session change has not already replaced. A transition
    // latched while this attachment was ending is the truer answer, and
    // overwriting it with "gone" would drop the latch ADR 0088 §2 is built on.
    screen.with_screen(|screen| {
        if screen.served_session().is_some() {
            screen.agent_gone();
        }
    });
    // Whatever the process left in the mapping is not the current picture of
    // anything, and after a fast user switch it is another person's screen
    // (ADR 0088 §2). Cleared before the process is stopped, so there is no
    // moment in which a reader could find it with nobody serving it.
    mapping.clear();
    // `stop` rather than letting the handle drop: dropping stops *watching* a
    // process, which would leave it serving a desktop this host no longer
    // believes in.
    match process.into_inner() {
        Ok(process) => process.stop(),
        // A poisoned mutex means the watchdog panicked holding it. The process
        // is still live, so it is still stopped.
        Err(poisoned) => poisoned.into_inner().stop(),
    }
    outcome
}

/// Pumps events from an attached process until the channel or the attachment
/// ends.
fn pump(
    screen: &Arc<AgentScreen>,
    mut link: AgentLink,
    attached: &AtomicBool,
    stopping: &AtomicBool,
) -> Served {
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
            // be waiting for a process that cannot be told anything.
            break;
        }
        if stopping.load(Ordering::SeqCst) || !attached.load(Ordering::SeqCst) {
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
            tracing::warn!("the attached process's channel stopped accepting commands");
            return;
        }
    }
}

/// Watches the attached process and the session notifications, and ends the
/// attachment when either says it is over.
///
/// The connect at the end is the point. `ConnectNamedPipe` and a blocking
/// `read_exact` are both waits no flag interrupts, and the established way to
/// break one in this codebase is to connect to the pipe — `windows_service.rs`
/// does exactly this to its own. A connection from this process is not the
/// one that was launched, so the accept loop turns it away, and on the next
/// round it asks `keep_waiting` and gives up.
fn watch<A: Attachment>(
    screen: &Arc<AgentScreen>,
    process: &std::sync::Mutex<A>,
    attached: &AtomicBool,
    stopping: &AtomicBool,
) {
    /// One look at the process: still running, and still the console
    /// session's.
    fn still_serving<A: Attachment>(process: &std::sync::Mutex<A>) -> bool {
        let process = process
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !process.is_alive() {
            tracing::info!(pid = process.pid(), "the attached process exited");
            return false;
        }
        // A fast user switch leaves the process alive in a session that is no
        // longer the console one. Its pixels belong to somebody who is no
        // longer at the machine, so the attachment ends rather than carrying
        // on showing them. The session notification says the same thing
        // sooner; this is what still says it under `--console`, which has none.
        if !process.serves_the_console() {
            tracing::info!(
                pid = process.pid(),
                "the attached process no longer serves the console session"
            );
            return false;
        }
        true
    }

    while attached.load(Ordering::SeqCst) && !stopping.load(Ordering::SeqCst) {
        // A session change ends the attachment even while the process is
        // perfectly healthy: an agent does not know it has been locked, and a
        // logon screen does not know somebody just signed in behind it.
        if screen.with_screen(fold_session_changes) || !still_serving(process) {
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
