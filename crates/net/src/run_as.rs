//! Whose name the host's disk is touched in (ADR 0169 §6).
//!
//! A host that *is* the person at the machine — the desktop client — reads and
//! writes as itself, and always has. A `LocalSystem` host — the session host —
//! must not: a guest holding `file_browse` would then see and change more of
//! this machine than the person sitting at it can. So every filesystem call the
//! transfer engine makes for a guest goes through a [`DiskAs`], which says who
//! makes it:
//!
//! - [`DiskAs::ThisProcess`] — the process itself, as before;
//! - [`DiskAs::Person`] — a [`RunAs`] that takes the signed-in person's
//!   identity on a thread of its own for exactly as long as the call runs;
//! - [`DiskAs::Nobody`] — nobody is signed in (the logon screen), there is no
//!   person to act as, and the call is refused rather than made as `SYSTEM`.
//!
//! Windows checks access when a path is opened, not when a handle that was
//! opened is read or written. So what has to run in the person's name is every
//! call that *names a path* — an open, a create, a listing, a rename, a delete;
//! streaming bytes through a handle opened that way afterwards does not, and
//! stays on the async runtime where it was.

use std::io;
use std::sync::Arc;

/// Something that can run a piece of blocking work as another identity.
///
/// Implemented by the session host over the signed-in person's token; tests
/// implement it to see which calls went through it.
pub trait RunAs: Send + Sync + std::fmt::Debug {
    /// Runs `work` as this identity and returns once it has finished.
    ///
    /// The identity is taken on a thread that gives it up before this
    /// returns — never on a thread of an async runtime's pool, which would go
    /// on to run somebody else's task in it.
    ///
    /// # Errors
    /// When the identity could not be taken. `work` has then not run.
    fn run(&self, work: &mut (dyn FnMut() + Send)) -> io::Result<()>;
}

/// Who a filesystem call on the host's behalf is made as (ADR 0169 §6).
#[derive(Debug, Clone, Default)]
pub enum DiskAs {
    /// This process: it is the person at the machine, or the work is its own.
    #[default]
    ThisProcess,
    /// The signed-in person, through the given identity.
    Person(Arc<dyn RunAs>),
    /// Nobody is signed in: every call is refused.
    Nobody,
}

impl DiskAs {
    /// Runs blocking `work` as this identity, on the calling thread or on one
    /// the identity is taken on.
    ///
    /// # Errors
    /// Whatever `work` returns; [`io::ErrorKind::PermissionDenied`] when there
    /// is nobody to act as, or the person's identity could not be taken.
    pub fn blocking<T: Send>(&self, work: impl FnOnce() -> io::Result<T> + Send) -> io::Result<T> {
        match self {
            Self::ThisProcess => work(),
            Self::Nobody => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "nobody is signed in to act as",
            )),
            Self::Person(person) => {
                let mut work = Some(work);
                let mut out = None;
                person
                    .run(&mut || {
                        if let Some(work) = work.take() {
                            out = Some(work());
                        }
                    })
                    .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error))?;
                out.unwrap_or_else(|| Err(io::Error::other("the work never ran")))
            }
        }
    }

    /// [`Self::blocking`] off the async runtime, on its blocking pool.
    ///
    /// # Errors
    /// As [`Self::blocking`], and when the blocking task did not finish.
    pub async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        let disk = self.clone();
        tokio::task::spawn_blocking(move || disk.blocking(work))
            .await
            .map_err(|_| io::Error::other("the filesystem task did not finish"))?
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Mutex;

    use super::*;

    /// A person that runs work on a thread of its own, the way the session
    /// host's does, and remembers how many times it was asked.
    #[derive(Debug, Default)]
    pub(crate) struct CountingPerson {
        pub(crate) calls: Mutex<usize>,
        pub(crate) threads: Mutex<Vec<std::thread::ThreadId>>,
    }

    impl RunAs for CountingPerson {
        fn run(&self, work: &mut (dyn FnMut() + Send)) -> io::Result<()> {
            *self.calls.lock().unwrap() += 1;
            std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        self.threads
                            .lock()
                            .unwrap()
                            .push(std::thread::current().id());
                        work();
                    })
                    .join()
            })
            .map_err(|_| io::Error::other("the person's thread panicked"))
        }
    }

    /// A person whose identity can never be taken.
    #[derive(Debug)]
    struct Unavailable;

    impl RunAs for Unavailable {
        fn run(&self, _work: &mut (dyn FnMut() + Send)) -> io::Result<()> {
            Err(io::Error::other("no token"))
        }
    }

    /// The process itself runs work where it is asked to; a person runs it on
    /// a thread of its own and hands the answer back.
    #[test]
    fn work_runs_as_whoever_the_disk_is_touched_as() {
        let caller = std::thread::current().id();
        let here = DiskAs::ThisProcess
            .blocking(|| Ok(std::thread::current().id()))
            .unwrap();
        assert_eq!(here, caller);

        let person = Arc::new(CountingPerson::default());
        let disk = DiskAs::Person(person.clone());
        let there = disk.blocking(|| Ok(std::thread::current().id())).unwrap();
        assert_ne!(
            there, caller,
            "the person's work ran on the caller's thread"
        );
        assert_eq!(*person.calls.lock().unwrap(), 1);
        assert_eq!(person.threads.lock().unwrap().as_slice(), &[there]);
    }

    /// Nobody signed in, or a person whose identity cannot be taken, is a
    /// refusal — and the work never runs as this process instead.
    #[test]
    fn with_nobody_to_act_as_nothing_runs() {
        let ran = std::sync::atomic::AtomicBool::new(false);
        for disk in [DiskAs::Nobody, DiskAs::Person(Arc::new(Unavailable))] {
            let error = disk
                .blocking(|| {
                    ran.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// The async form keeps the work off the runtime's own threads.
    #[tokio::test]
    async fn the_async_form_runs_off_the_runtime() {
        let person = Arc::new(CountingPerson::default());
        let disk = DiskAs::Person(person.clone());
        let answer = disk.run(|| Ok(7)).await.unwrap();
        assert_eq!(answer, 7);
        assert_eq!(*person.calls.lock().unwrap(), 1);
    }
}
