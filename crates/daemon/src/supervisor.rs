//! Task supervision.
//!
//! The supervisor owns every task's handle and observes its exit. That is the
//! whole point: a restart limiter a failure can slip past is not a limiter, and
//! the only way to make it unslippable is to hold the handle rather than to
//! remember to check something.
//!
//! The three error classes arrive by two routes, which is why none of them can
//! be forgotten. A *transient* failure is returned and retried with growing
//! backoff. A *permanent* failure is returned and stops that task; both are
//! variants of one enum and are matched exhaustively. A *bug* is a panic, and a
//! panic arrives as a join failure on the handle, so it cannot be returned,
//! ignored, or caught by a task that would rather not mention it.
//!
//! **What happens to a task is told as it happens**, to whatever
//! [`Supervisor::telling`] was handed: a [`Notice`] for a task that is started
//! again and for one that stopped. [`Supervisor::run`] returns its records
//! only once every task has stopped, and the tasks of a daemon do not all
//! stop.
//!
//! This module reads the time from `tokio::time` rather than from an injected
//! `benshi_core::clock::Clock`, and it is the only place in the workspace that
//! does. It has to sleep on the same clock it measures with, and a `Clock`
//! cannot sleep, so taking one as a parameter would hand this module two clocks
//! free to disagree about whether a task had been running long enough. What the
//! rule is for still holds: `tokio::time` is virtualised under a paused runtime,
//! so `a_long_backoff_does_not_count_as_work` winds through 2223 seconds of
//! retries and takes no measurable time to run.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tokio::task::{Id, JoinSet};
use tokio::time::Instant;

/// How many times in a row a task is restarted after a panic before it is left
/// stopped.
///
/// A panic is a bug, and a bug that fires on every attempt will fire on the
/// next one too. Restarting a few times covers the case where the bug needed a
/// particular input that has since passed; going on forever turns a crash loop
/// into a busy loop that hides it.
///
/// In a row, and not in total: an attempt that ran for [`PROGRESS_INTERVAL`]
/// before it failed starts the count again, whether it ended in a panic or in
/// a transient failure. A budget spent once and never renewed would stop a
/// task for the life of the process over a minute of trouble it had long
/// since recovered from.
pub const RESTART_LIMIT: u32 = 5;

/// How long to wait before the first retry of a transient failure.
pub const BACKOFF_BASE: Duration = Duration::from_secs(1);

/// The longest a retry is ever delayed.
///
/// A transient failure is something that is expected to pass: a bus that is
/// restarting, a network that is down. Doubling without a ceiling would leave a
/// task asleep for hours after an outage it has long recovered from.
pub const BACKOFF_CEILING: Duration = Duration::from_secs(60);

/// How long a task must run before a failure counts as new trouble rather
/// than the continuation of the trouble before it.
///
/// [`RESTART_LIMIT`] bounds panics in a row, and the wait before a retry
/// doubles over transient failures in a row. In a row has to mean something
/// in time: a failure that ends an attempt which ran this long starts both
/// rows again, whichever kind of failure it is. Such an attempt did work, so
/// what ended it is new trouble.
///
/// A panic is restarted with no delay, so one bad millisecond can spend the
/// whole of [`RESTART_LIMIT`]. Counted over the life of a task, the next
/// panic would then stop it however long it had worked since, and every
/// transient failure from the seventh on would wait [`BACKOFF_CEILING`].
///
/// A minute, because the shortest-lived thing this will supervise polls once a
/// second, so a minute is sixty rounds of real work.
///
/// It equals [`BACKOFF_CEILING`] and is not derived from it: neither number
/// follows from the other and changing one must not change the other. The
/// equality is worth knowing about rather than ignoring, because it is the
/// worst case for this rule. A task retried at the ceiling sleeps for exactly
/// this long, so an attempt that woke and panicked at once would look like a
/// full interval of work. Subtracting the delay before comparing is what stops
/// it, and `a_long_backoff_does_not_count_as_work` is what holds that there.
pub const PROGRESS_INTERVAL: Duration = Duration::from_secs(60);

/// Why a task failed, in the two classes a task can report about itself.
///
/// The third class, a bug, is not here and cannot be: it arrives as a panic on
/// the handle the supervisor owns.
#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    /// Expected to pass on its own. Retried with backoff.
    #[error(transparent)]
    Transient(anyhow::Error),
    /// Will not pass on its own. Stops the task.
    #[error(transparent)]
    Permanent(anyhow::Error),
}

/// Why a task is no longer running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stopped {
    /// It finished what it was doing and returned.
    Finished,
    /// A failure that will not pass, rendered here for a person. Usually one
    /// the task reported; a task cancelled from outside is recorded here too,
    /// because it is not coming back either.
    Permanent(String),
    /// It kept panicking. The last panic is rendered here.
    Exhausted(String),
}

// Written so that what the binary says of a task that stopped reads as a
// sentence. `{:?}` would say `Exhausted("...")`: a variant name and a quoted
// string, which is a debugger's view of a value rather than an account of what
// happened.
impl fmt::Display for Stopped {
    fn fmt(&self, into: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Finished => into.write_str("finished and returned"),
            Self::Permanent(why) => write!(into, "stopped for good: {why}"),
            Self::Exhausted(last) => write!(into, "kept panicking and was given up on: {last}"),
        }
    }
}

/// What became of one supervised task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRecord {
    /// The name it was supervised under.
    pub name: String,
    /// How many times it was started again after failing.
    pub restarts: u32,
    /// Why it is no longer running.
    pub stopped_by: Stopped,
}

/// What the supervisor tells of a task, as it happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// The task failed in a way that is expected to pass, and is started
    /// again after a wait.
    Retried {
        /// The name the task is supervised under.
        name: String,
        /// How long the task waits before it starts.
        after: Duration,
        /// What the task failed with, rendered for a person.
        why: String,
    },
    /// The task panicked, and is started again at once.
    Restarted {
        /// The name the task is supervised under.
        name: String,
        /// What the panic said.
        why: String,
    },
    /// The task is no longer running and is not started again.
    Stopped(TaskRecord),
}

// Each is a line of its own on a terminal, after the name of the program.
impl fmt::Display for Notice {
    fn fmt(&self, into: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Retried { name, after, why } => {
                write!(
                    into,
                    "{name} failed and is started again in {after:?}: {why}"
                )
            }
            Self::Restarted { name, why } => {
                write!(into, "{name} panicked and is started again: {why}")
            }
            Self::Stopped(record) => write!(into, "{} {}", record.name, record.stopped_by),
        }
    }
}

/// One task's body, ready to be run again.
type TaskFuture = Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send>>;

/// A set of tasks whose handles are owned here and nowhere else.
pub struct Supervisor {
    tasks: Vec<Supervised>,
    tell: Box<dyn FnMut(&Notice) + Send>,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::telling(|_notice| {})
    }
}

/// One task and what has happened to it so far.
struct Supervised {
    name: String,
    body: Box<dyn FnMut() -> TaskFuture + Send>,
    restarts: u32,
    /// Panics in a row. The row starts again at the failure that ends an
    /// attempt which worked for [`PROGRESS_INTERVAL`], whichever kind that
    /// failure is.
    panics: u32,
    /// Transient failures in a row, by the same rule.
    faults: u32,
    /// When the current attempt was spawned, and how much of that it spent
    /// asleep before starting. The difference is how long it actually ran,
    /// which is what decides whether a failure continues the rows before it.
    last_start: Option<Instant>,
    last_delay: Duration,
    /// `None` for as long as it is still running.
    stopped_by: Option<Stopped>,
}

impl Supervised {
    /// Whether the attempt that has just ended ran for long enough to have
    /// got work done.
    ///
    /// The wait before it started is taken off, because waiting is not
    /// working. Absent a start time nothing has run, which counts as no
    /// work.
    fn worked(&self) -> bool {
        let ran_for = self
            .last_start
            .map(|start| start.elapsed().saturating_sub(self.last_delay))
            .unwrap_or_default();

        ran_for >= PROGRESS_INTERVAL
    }
}

/// How long to wait after the `faults`-th transient failure in a row.
///
/// Doubling, from [`BACKOFF_BASE`] and never past [`BACKOFF_CEILING`]. Only a
/// transient failure is delayed: it is retried without limit, so it needs the
/// throttle. A panic is retried at most [`RESTART_LIMIT`] times, and a bounded
/// retry cannot run away.
fn backoff(faults: u32) -> Duration {
    // `faults` is unbounded, and an exponent large enough to overflow would
    // wrap to a short delay, turning the throttle into its opposite. The
    // clamp is what prevents that; the saturating forms only keep it
    // prevented if the constants above ever change.
    let doubling = 2_u32.saturating_pow(faults.saturating_sub(1).min(16));

    BACKOFF_BASE.saturating_mul(doubling).min(BACKOFF_CEILING)
}

/// What a panic said, as far as it can be recovered.
fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    // `panic!` with a literal carries a `&'static str` and one with arguments
    // carries a `String`, so both are tried before giving up. Each downcast
    // hands the box back when it does not match, so nothing is copied.
    match panic.downcast::<&'static str>() {
        Ok(literal) => (*literal).to_owned(),
        Err(panic) => match panic.downcast::<String>() {
            Ok(formatted) => *formatted,
            Err(_something_else) => "a panic carrying no message".to_owned(),
        },
    }
}

/// Start one task, after `delay`, and record which task the handle belongs to.
fn start(
    set: &mut JoinSet<Result<(), TaskError>>,
    owners: &mut HashMap<Id, usize>,
    task: &mut Supervised,
    index: usize,
    delay: Duration,
) {
    let body = (task.body)();
    task.last_start = Some(Instant::now());
    task.last_delay = delay;
    let handle = set.spawn(async move {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }

        body.await
    });

    owners.insert(handle.id(), index);
}

impl Supervisor {
    /// A supervisor with nothing to supervise yet, which tells nobody of its
    /// tasks.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A supervisor with nothing to supervise yet, which tells `tell` of a
    /// task as the task is started again and as it stops.
    ///
    /// `tell` is called from [`Supervisor::run`] itself. A task that ends
    /// while `tell` is at work is started again, or recorded as stopped, only
    /// once `tell` has returned. So a `tell` that waits keeps every task that
    /// ends waiting with it, and one that writes waits where nobody reads.
    /// [`Voice::say`](crate::voice::Voice::say) hands a line over and
    /// returns.
    #[must_use]
    pub fn telling(tell: impl FnMut(&Notice) + Send + 'static) -> Self {
        Self {
            tasks: Vec::new(),
            tell: Box::new(tell),
        }
    }

    /// Take a task, to be started when the supervisor runs.
    ///
    /// `body` is called again for each restart, so it produces a fresh future
    /// rather than being one. A task that cannot be started twice cannot be
    /// supervised.
    pub fn supervise<B, F>(&mut self, name: &str, mut body: B)
    where
        B: FnMut() -> F + Send + 'static,
        F: Future<Output = Result<(), TaskError>> + Send + 'static,
    {
        self.tasks.push(Supervised {
            name: name.to_owned(),
            body: Box::new(move || Box::pin(body())),
            restarts: 0,
            panics: 0,
            faults: 0,
            last_start: None,
            last_delay: Duration::ZERO,
            stopped_by: None,
        });
    }

    /// Run every task until none is left running.
    ///
    /// Returns one record per task, in the order they were supervised. In the
    /// daemon the tasks do not finish, so this does not return; in a test they
    /// do, which is what makes the supervisor observable at all.
    ///
    /// # Panics
    ///
    /// If a handle is joined that this supervisor did not spawn, or if a task
    /// ends without a reason being written down. Both are bugs in this file and
    /// neither is reachable from a task: a task that panics is caught and
    /// recorded, which is the point of owning the handle.
    pub async fn run(self) -> Vec<TaskRecord> {
        let Self {
            mut tasks,
            mut tell,
        } = self;
        let mut set = JoinSet::new();
        let mut owners = HashMap::new();

        for (index, task) in tasks.iter_mut().enumerate() {
            start(&mut set, &mut owners, task, index, Duration::ZERO);
        }

        while let Some(joined) = set.join_next_with_id().await {
            let (index, outcome) = match joined {
                Ok((id, outcome)) => (owners.remove(&id), Ok(outcome)),
                Err(failure) => (owners.remove(&failure.id()), Err(failure)),
            };
            // Every handle was recorded as it was spawned, so a handle with no
            // owner is a bug in this file rather than a failure of a task.
            let index = index.expect("a joined handle was spawned here");
            let task = &mut tasks[index];
            // Work ends both rows, whichever way the attempt then ended.
            if task.worked() {
                task.panics = 0;
                task.faults = 0;
            }

            // A task that is started again is told of before it is started.
            let stopped_by = match outcome {
                Ok(Ok(())) => Some(Stopped::Finished),
                Ok(Err(TaskError::Permanent(reason))) => {
                    Some(Stopped::Permanent(reason.to_string()))
                }
                Ok(Err(TaskError::Transient(reason))) => {
                    task.faults += 1;
                    task.restarts += 1;
                    let delay = backoff(task.faults);
                    tell(&Notice::Retried {
                        name: task.name.clone(),
                        after: delay,
                        why: reason.to_string(),
                    });
                    start(&mut set, &mut owners, task, index, delay);
                    None
                }
                Err(failure) if failure.is_panic() => {
                    let reason = panic_message(failure.into_panic());
                    if task.panics >= RESTART_LIMIT {
                        Some(Stopped::Exhausted(reason))
                    } else {
                        task.panics += 1;
                        task.restarts += 1;
                        tell(&Notice::Restarted {
                            name: task.name.clone(),
                            why: reason,
                        });
                        start(&mut set, &mut owners, task, index, Duration::ZERO);
                        None
                    }
                }
                // Nothing here aborts a task and the set outlives the loop, so
                // a cancellation means something else reached in and took the
                // handle. Recorded rather than guessed at.
                Err(_cancelled) => Some(Stopped::Permanent("the task was cancelled".to_owned())),
            };

            if let Some(stopped_by) = stopped_by {
                tell(&Notice::Stopped(TaskRecord {
                    name: task.name.clone(),
                    restarts: task.restarts,
                    stopped_by: stopped_by.clone(),
                }));
                task.stopped_by = Some(stopped_by);
            }
        }

        tasks
            .into_iter()
            .map(|task| TaskRecord {
                name: task.name,
                restarts: task.restarts,
                // The loop ends when no handle is left, so every task has
                // stopped and every reason has been written down.
                stopped_by: task.stopped_by.expect("every task that ended recorded why"),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BACKOFF_BASE, BACKOFF_CEILING, Notice, PROGRESS_INTERVAL, RESTART_LIMIT, Stopped,
        Supervisor, TaskError, TaskRecord, backoff,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Mutex, PoisonError};
    use std::time::Duration;
    use tokio::time::Instant;

    /// How long a test waits to be told, on the clock the supervisor sleeps
    /// on. Only ever spent on a failure.
    const PATIENCE: Duration = Duration::from_secs(5);

    /// A counter shared between a test and the task body it supervises.
    fn counter() -> Arc<AtomicU32> {
        Arc::new(AtomicU32::new(0))
    }

    /// Count this attempt and return which one it was, starting at one.
    fn attempt(count: &Arc<AtomicU32>) -> u32 {
        count.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// What a supervisor told, in the order it told it.
    #[derive(Clone, Default)]
    struct Told(Arc<Mutex<Vec<Notice>>>);

    impl Told {
        /// What a supervisor is handed so that what it tells is kept here.
        fn listener(&self) -> impl FnMut(&Notice) + Send + 'static {
            let told = self.clone();

            move |notice| {
                told.0
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(notice.clone());
            }
        }

        fn all(&self) -> Vec<Notice> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        /// Wait until this many notices have been told.
        ///
        /// Slept for and never yielded for: a paused clock moves on only
        /// while every task waits on it.
        async fn until(&self, count: usize) {
            let waited = tokio::time::timeout(PATIENCE, async {
                while self.all().len() < count {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await;

            assert!(waited.is_ok(), "told {:?} and no more", self.all());
        }
    }

    /// What a task is stopped by, under this name and after this many
    /// restarts, as a supervisor tells of it.
    fn stopped(name: &str, restarts: u32, stopped_by: Stopped) -> Notice {
        Notice::Stopped(TaskRecord {
            name: name.to_owned(),
            restarts,
            stopped_by,
        })
    }

    #[tokio::test(start_paused = true)]
    async fn a_task_that_stops_for_good_is_told_of_while_another_runs_on() {
        // The daemon's socket does not stop, so `run` hands nothing back while
        // the daemon runs.
        let told = Told::default();
        let mut supervisor = Supervisor::telling(told.listener());
        supervisor.supervise("refused", || async {
            Err(TaskError::Permanent(anyhow::anyhow!("the token expired")))
        });
        supervisor.supervise("steady", std::future::pending);

        let running = tokio::spawn(supervisor.run());
        told.until(1).await;

        assert!(!running.is_finished(), "the other task had not stopped");
        assert_eq!(
            told.all(),
            [stopped(
                "refused",
                0,
                Stopped::Permanent("the token expired".to_owned())
            )]
        );
        running.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_task_started_again_after_a_fault_is_told_of_before_it_starts() {
        let told = Told::default();
        let told_by_then: Arc<Mutex<Vec<usize>>> = Arc::default();
        let starts: Arc<Mutex<Vec<Instant>>> = Arc::default();
        let mut supervisor = Supervisor::telling(told.listener());
        supervisor.supervise("retrying", {
            let told = told.clone();
            let told_by_then = Arc::clone(&told_by_then);
            let starts = Arc::clone(&starts);
            move || {
                // Read where the attempt is made ready, which the supervisor
                // does as it starts it. Read where the attempt runs, a
                // notice told after the start would be there as well.
                told_by_then
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(told.all().len());
                let starts = Arc::clone(&starts);
                async move {
                    let attempt = {
                        let mut starts = starts.lock().unwrap_or_else(PoisonError::into_inner);
                        starts.push(Instant::now());
                        starts.len()
                    };

                    if attempt < 4 {
                        Err(TaskError::Transient(anyhow::anyhow!("the bus is away")))
                    } else {
                        Ok(())
                    }
                }
            }
        });

        supervisor.run().await;

        let again = |after| Notice::Retried {
            name: "retrying".to_owned(),
            after,
            why: "the bus is away".to_owned(),
        };
        assert_eq!(
            told.all(),
            [
                again(BACKOFF_BASE),
                again(BACKOFF_BASE * 2),
                again(BACKOFF_BASE * 4),
                stopped("retrying", 3, Stopped::Finished),
            ]
        );
        assert_eq!(
            *told_by_then.lock().unwrap_or_else(PoisonError::into_inner),
            [0, 1, 2, 3],
            "an attempt had started before it was told of"
        );
        // The wait that is told is the wait that is made.
        let starts = starts.lock().unwrap_or_else(PoisonError::into_inner);
        let waits: Vec<Duration> = starts.windows(2).map(|pair| pair[1] - pair[0]).collect();
        assert_eq!(waits, [BACKOFF_BASE, BACKOFF_BASE * 2, BACKOFF_BASE * 4]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_task_started_again_after_a_panic_is_told_of_before_it_starts() {
        let told = Told::default();
        let told_by_then: Arc<Mutex<Vec<usize>>> = Arc::default();
        let mut supervisor = Supervisor::telling(told.listener());
        supervisor.supervise("flaky", {
            let told = told.clone();
            let told_by_then = Arc::clone(&told_by_then);
            move || {
                // Read where the attempt is made ready, as in the test above.
                let attempt = {
                    let mut by_then = told_by_then.lock().unwrap_or_else(PoisonError::into_inner);
                    by_then.push(told.all().len());
                    by_then.len()
                };
                async move {
                    assert!(attempt > 1, "the first attempt panics");
                    Ok(())
                }
            }
        });

        supervisor.run().await;

        assert_eq!(
            told.all(),
            [
                Notice::Restarted {
                    name: "flaky".to_owned(),
                    why: "the first attempt panics".to_owned(),
                },
                stopped("flaky", 1, Stopped::Finished),
            ]
        );
        assert_eq!(
            *told_by_then.lock().unwrap_or_else(PoisonError::into_inner),
            [0, 1],
            "an attempt had started before it was told of"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_task_given_up_on_is_told_of() {
        let attempts = counter();
        let told = Told::default();
        let mut supervisor = Supervisor::telling(told.listener());
        supervisor.supervise("doomed", {
            let attempts = Arc::clone(&attempts);
            move || {
                let attempts = Arc::clone(&attempts);
                async move {
                    // The same escape the test of the limit has.
                    assert!(
                        attempt(&attempts) > RESTART_LIMIT + 10,
                        "this one always panics"
                    );
                    Ok(())
                }
            }
        });

        supervisor.run().await;

        let told = told.all();
        let restarted = Notice::Restarted {
            name: "doomed".to_owned(),
            why: "this one always panics".to_owned(),
        };
        let (last, before) = told.split_last().expect("something was told");
        assert_eq!(before, vec![restarted; RESTART_LIMIT as usize]);
        assert_eq!(
            *last,
            stopped(
                "doomed",
                RESTART_LIMIT,
                Stopped::Exhausted("this one always panics".to_owned())
            )
        );
    }

    /// The waits a supervisor told of, in the order it told of them.
    fn waits_in(told: &Told) -> Vec<Duration> {
        told.all()
            .iter()
            .filter_map(|notice| match notice {
                Notice::Retried { after, .. } => Some(*after),
                Notice::Restarted { .. } | Notice::Stopped(_) => None,
            })
            .collect()
    }

    /// How an attempt of a task fails.
    #[derive(Clone, Copy)]
    enum Failure {
        /// In a way that passes.
        Fault,
        /// With a panic.
        Panic,
    }

    /// How long an attempt works for that fails as it starts.
    const AT_ONCE: Duration = Duration::ZERO;

    /// What a supervisor told of one task that fails once for each of
    /// `failures`, in their order and each after working for that long, and
    /// returns at the attempt after the last.
    async fn failing(failures: Vec<(Duration, Failure)>) -> Told {
        let told = Told::default();
        let mut supervisor = Supervisor::telling(told.listener());
        supervisor.supervise("troubled", {
            let mut failures = failures.into_iter();
            move || {
                let failure = failures.next();
                async move {
                    let Some((worked, failure)) = failure else {
                        return Ok(());
                    };

                    tokio::time::sleep(worked).await;
                    match failure {
                        Failure::Fault => {
                            Err(TaskError::Transient(anyhow::anyhow!("the bus is away")))
                        }
                        Failure::Panic => panic!("index out of bounds"),
                    }
                }
            }
        });

        supervisor.run().await;

        told
    }

    #[tokio::test(start_paused = true)]
    async fn a_fault_after_a_while_of_work_waits_as_the_first_did() {
        // A task that ran for a month and meets a fault has not been failing
        // for a month. Counted from the first fault of its life, every fault
        // from the seventh on waits a minute.
        let told = failing(vec![
            (AT_ONCE, Failure::Fault),
            (AT_ONCE, Failure::Fault),
            (AT_ONCE, Failure::Fault),
            (PROGRESS_INTERVAL, Failure::Fault),
            (AT_ONCE, Failure::Fault),
        ])
        .await;

        assert_eq!(
            waits_in(&told),
            [
                BACKOFF_BASE,
                BACKOFF_BASE * 2,
                BACKOFF_BASE * 4,
                BACKOFF_BASE,
                BACKOFF_BASE * 2,
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_fault_short_of_a_while_of_work_waits_longer_than_the_last() {
        let short = PROGRESS_INTERVAL.saturating_sub(Duration::from_millis(1));

        let told = failing(vec![
            (AT_ONCE, Failure::Fault),
            (AT_ONCE, Failure::Fault),
            (AT_ONCE, Failure::Fault),
            (short, Failure::Fault),
            (AT_ONCE, Failure::Fault),
        ])
        .await;

        assert_eq!(
            waits_in(&told),
            [
                BACKOFF_BASE,
                BACKOFF_BASE * 2,
                BACKOFF_BASE * 4,
                BACKOFF_BASE * 8,
                BACKOFF_BASE * 16,
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_wait_is_not_work() {
        // An attempt that waited a minute to start and failed at once has
        // been going for a minute and has done nothing. Taken for work, the
        // wait would fall from its ceiling to its base and climb again.
        let told = failing(vec![(AT_ONCE, Failure::Fault); 9]).await;

        let waits = waits_in(&told);
        assert_eq!(waits.len(), 9, "{waits:?}");
        assert_eq!(
            waits[6..],
            [BACKOFF_CEILING, BACKOFF_CEILING, BACKOFF_CEILING],
            "{waits:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn work_that_a_fault_ends_gives_the_budget_back() {
        // Every panic the budget allows, a while of work, and every panic the
        // budget allows again.
        let spent = vec![(AT_ONCE, Failure::Panic); RESTART_LIMIT as usize];
        let failures = [
            spent.clone(),
            vec![(PROGRESS_INTERVAL, Failure::Fault)],
            spent,
        ]
        .concat();

        let told = failing(failures).await;

        assert_eq!(
            told.all().last(),
            Some(&stopped(
                "troubled",
                RESTART_LIMIT * 2 + 1,
                Stopped::Finished
            ))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_panic_that_ends_work_is_the_first_of_a_new_row() {
        // Every panic the budget allows, a while of work that a panic ends,
        // and every panic the budget allows again. The panic that ends the
        // work is the first of its row, so the last of these is one more
        // than the budget allows.
        let spent = vec![(AT_ONCE, Failure::Panic); RESTART_LIMIT as usize];
        let failures = [
            spent.clone(),
            vec![(PROGRESS_INTERVAL, Failure::Panic)],
            spent,
        ]
        .concat();

        let told = failing(failures).await;

        assert_eq!(
            told.all().last(),
            Some(&stopped(
                "troubled",
                RESTART_LIMIT * 2,
                Stopped::Exhausted("index out of bounds".to_owned())
            ))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_fault_short_of_a_while_of_work_gives_no_budget_back() {
        let short = PROGRESS_INTERVAL.saturating_sub(Duration::from_millis(1));
        let mut failures = vec![(AT_ONCE, Failure::Panic); RESTART_LIMIT as usize];
        failures.push((short, Failure::Fault));
        failures.push((AT_ONCE, Failure::Panic));

        let told = failing(failures).await;

        assert_eq!(
            told.all().last(),
            Some(&stopped(
                "troubled",
                RESTART_LIMIT + 1,
                Stopped::Exhausted("index out of bounds".to_owned())
            ))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn work_that_a_panic_ends_starts_the_wait_again() {
        let told = failing(vec![
            (AT_ONCE, Failure::Fault),
            (AT_ONCE, Failure::Fault),
            (AT_ONCE, Failure::Fault),
            (PROGRESS_INTERVAL, Failure::Panic),
            (AT_ONCE, Failure::Fault),
        ])
        .await;

        assert_eq!(
            waits_in(&told),
            [
                BACKOFF_BASE,
                BACKOFF_BASE * 2,
                BACKOFF_BASE * 4,
                BACKOFF_BASE
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_panic_short_of_a_while_of_work_leaves_the_wait_growing() {
        let short = PROGRESS_INTERVAL.saturating_sub(Duration::from_millis(1));

        let told = failing(vec![
            (AT_ONCE, Failure::Fault),
            (AT_ONCE, Failure::Fault),
            (AT_ONCE, Failure::Fault),
            (short, Failure::Panic),
            (AT_ONCE, Failure::Fault),
        ])
        .await;

        assert_eq!(
            waits_in(&told),
            [
                BACKOFF_BASE,
                BACKOFF_BASE * 2,
                BACKOFF_BASE * 4,
                BACKOFF_BASE * 8
            ]
        );
    }

    #[test]
    fn a_notice_reads_as_a_sentence() {
        // Each is a line the binary prints under its own name.
        let retried = Notice::Retried {
            name: "detection".to_owned(),
            after: Duration::from_secs(2),
            why: "the bus is away".to_owned(),
        };
        let restarted = Notice::Restarted {
            name: "detection".to_owned(),
            why: "index out of bounds".to_owned(),
        };
        let gone = stopped(
            "detection",
            3,
            Stopped::Permanent("the token expired".to_owned()),
        );

        assert_eq!(
            retried.to_string(),
            "detection failed and is started again in 2s: the bus is away"
        );
        assert_eq!(
            restarted.to_string(),
            "detection panicked and is started again: index out of bounds"
        );
        assert_eq!(
            gone.to_string(),
            "detection stopped for good: the token expired"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_panicking_task_is_restarted() {
        let attempts = counter();
        let mut supervisor = Supervisor::new();
        supervisor.supervise("flaky", {
            let attempts = Arc::clone(&attempts);
            move || {
                let attempts = Arc::clone(&attempts);
                async move {
                    assert!(attempt(&attempts) > 1, "the first attempt panics");
                    Ok(())
                }
            }
        });

        let records = supervisor.run().await;

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(records[0].restarts, 1);
        assert_eq!(records[0].stopped_by, Stopped::Finished);
    }

    #[tokio::test(start_paused = true)]
    async fn a_task_that_keeps_panicking_is_stopped_after_the_limit() {
        // A bug that fires every time is not going to stop firing. Restarting
        // for ever would turn a crash into a busy loop that hides it.
        let attempts = counter();
        let mut supervisor = Supervisor::new();
        supervisor.supervise("doomed", {
            let attempts = Arc::clone(&attempts);
            move || {
                let attempts = Arc::clone(&attempts);
                async move {
                    // It stops panicking well past the limit, so a limit that
                    // stopped working fails this test rather than spinning for
                    // ever. Nothing sleeps between restarts after a panic.
                    assert!(
                        attempt(&attempts) > RESTART_LIMIT + 10,
                        "this one always panics"
                    );
                    Ok(())
                }
            }
        });

        let records = supervisor.run().await;

        assert_eq!(attempts.load(Ordering::SeqCst), RESTART_LIMIT + 1);
        assert_eq!(records[0].restarts, RESTART_LIMIT);
        assert!(matches!(records[0].stopped_by, Stopped::Exhausted(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_task_that_worked_for_a_while_gets_its_budget_back() {
        // Six panics inside one millisecond and six panics across an afternoon
        // are not the same trouble. Counting from a fixed origin makes them
        // identical, and a task that met a bad minute would then be stopped for
        // the life of the process while the daemon went on looking healthy.
        let attempts = counter();
        let mut supervisor = Supervisor::new();
        supervisor.supervise("long-lived", {
            let attempts = Arc::clone(&attempts);
            move || {
                let attempts = Arc::clone(&attempts);
                async move {
                    let attempt = attempt(&attempts);
                    tokio::time::sleep(PROGRESS_INTERVAL * 2).await;
                    // Well past twice the limit, so that a budget which does
                    // come back still leaves this test finite.
                    assert!(attempt > RESTART_LIMIT * 2 + 2, "panicking again");
                    Ok(())
                }
            }
        });

        let records = supervisor.run().await;

        assert!(
            attempts.load(Ordering::SeqCst) > RESTART_LIMIT + 1,
            "a budget counted from the first panic would have stopped it at {}",
            RESTART_LIMIT + 1
        );
        assert_eq!(records[0].stopped_by, Stopped::Finished);
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_backoff_does_not_count_as_work() {
        // A crash loop that waits a minute before each panic: it fails
        // transiently until the retry delay has grown to a minute, then
        // panics the instant the delay is over. Waiting is not working, so the
        // attempt is as short as it looks and the limit must still stop it.
        let ramp = (1..=32_u32)
            .find(|&restarts| backoff(restarts) >= PROGRESS_INTERVAL)
            .expect("the backoff ceiling reaches the progress interval");
        // Far past what a working limit needs, so a budget that does keep being
        // refunded still leaves this test finite.
        let escape = (ramp + 1) * RESTART_LIMIT * 8;
        let attempts = counter();
        let mut supervisor = Supervisor::new();
        supervisor.supervise("sleepy", {
            let attempts = Arc::clone(&attempts);
            move || {
                let attempts = Arc::clone(&attempts);
                async move {
                    let attempt = attempt(&attempts);
                    if attempt > escape {
                        return Ok(());
                    }

                    // As many transient failures as take the delay to its
                    // ceiling, then a panic, and the same again. A delay
                    // counted as work would start both counts again, so the
                    // delay has to be climbed before every panic.
                    let panics_now = attempt.is_multiple_of(ramp + 1);
                    if !panics_now {
                        return Err(TaskError::Transient(anyhow::anyhow!("the bus is away")));
                    }

                    panic!("it panics the instant it wakes");
                }
            }
        });

        let records = supervisor.run().await;

        assert!(
            matches!(records[0].stopped_by, Stopped::Exhausted(_)),
            "a delay counted as work refunds the budget for ever, got {:?}",
            records[0].stopped_by
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_permanent_error_stops_the_task_without_retrying() {
        let attempts = counter();
        let mut supervisor = Supervisor::new();
        supervisor.supervise("refused", {
            let attempts = Arc::clone(&attempts);
            move || {
                let attempts = Arc::clone(&attempts);
                async move {
                    // It gives up refusing after a few attempts, so a
                    // supervisor that retried a permanent failure fails this
                    // test rather than retrying for ever.
                    if attempt(&attempts) > 3 {
                        return Ok(());
                    }

                    Err(TaskError::Permanent(anyhow::anyhow!("the token expired")))
                }
            }
        });

        let records = supervisor.run().await;

        assert_eq!(attempts.load(Ordering::SeqCst), 1, "it must not try again");
        assert_eq!(records[0].restarts, 0);
    }

    #[test]
    fn an_outcome_reads_as_an_account_rather_than_a_value() {
        // The binary says this of a task that stopped, where `{:?}` would say
        // `Exhausted("...")`. The match below is exhaustive and empty: a
        // variant added to the enum stops the crate compiling here, in front of
        // the list it has to join.
        let outcomes = [
            Stopped::Finished,
            Stopped::Permanent("the session bus went away".to_owned()),
            Stopped::Exhausted("index out of bounds".to_owned()),
        ];

        for outcome in &outcomes {
            match outcome {
                Stopped::Finished | Stopped::Permanent(_) | Stopped::Exhausted(_) => {}
            }

            let said = outcome.to_string();
            assert!(
                !said.contains("Stopped") && !said.contains('"'),
                "a value reached a terminal instead of an account: {said}"
            );
        }

        assert_eq!(
            Stopped::Exhausted("index out of bounds".to_owned()).to_string(),
            "kept panicking and was given up on: index out of bounds"
        );
        assert_eq!(Stopped::Finished.to_string(), "finished and returned");
    }

    #[tokio::test(start_paused = true)]
    async fn a_stopped_task_records_the_error_that_stopped_it() {
        // The record has to exist from the first commit. By the time a command
        // is written to show it, the information is long gone.
        let attempts = counter();
        let mut supervisor = Supervisor::new();
        supervisor.supervise("refused", {
            let attempts = Arc::clone(&attempts);
            move || {
                let attempts = Arc::clone(&attempts);
                async move {
                    if attempt(&attempts) > 3 {
                        return Ok(());
                    }

                    Err(TaskError::Permanent(anyhow::anyhow!("the token expired")))
                }
            }
        });

        let records = supervisor.run().await;

        match &records[0].stopped_by {
            Stopped::Permanent(reason) => assert!(reason.contains("the token expired")),
            other => panic!("expected the reason to be kept, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_transient_error_is_retried_with_growing_backoff() {
        // Measured on tokio's paused clock, so the growth is observable without
        // the test taking as long as the backoff it is checking.
        let starts: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
        let mut supervisor = Supervisor::new();
        supervisor.supervise("retrying", {
            let starts = Arc::clone(&starts);
            move || {
                let starts = Arc::clone(&starts);
                async move {
                    let attempt = {
                        let mut starts = starts.lock().unwrap_or_else(PoisonError::into_inner);
                        starts.push(Instant::now());
                        starts.len()
                    };

                    if attempt < 4 {
                        Err(TaskError::Transient(anyhow::anyhow!("the bus is away")))
                    } else {
                        Ok(())
                    }
                }
            }
        });

        let records = supervisor.run().await;

        let starts = starts.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(starts.len(), 4);
        let first = starts[1] - starts[0];
        let second = starts[2] - starts[1];
        let third = starts[3] - starts[2];
        assert!(
            second > first && third > second,
            "each wait must exceed the last, got {first:?}, {second:?}, {third:?}"
        );
        assert_eq!(records[0].stopped_by, Stopped::Finished);
    }

    #[tokio::test(start_paused = true)]
    async fn one_task_panicking_does_not_disturb_another() {
        // The reason a panic must not take the process down: the other half of
        // the daemon has nothing to do with it and is still working.
        let steady = counter();
        let panicking = counter();
        let mut supervisor = Supervisor::new();

        supervisor.supervise("panicking", {
            let panicking = Arc::clone(&panicking);
            move || {
                let panicking = Arc::clone(&panicking);
                async move {
                    // The same escape the limit test uses. With the limit
                    // working this is never reached; with it broken, the
                    // difference between a hang and a failure belongs to the
                    // test that owns the limit, not to this one.
                    assert!(attempt(&panicking) > RESTART_LIMIT + 10, "unrelated");
                    Ok(())
                }
            }
        });
        supervisor.supervise("steady", {
            let steady = Arc::clone(&steady);
            move || {
                let steady = Arc::clone(&steady);
                async move {
                    attempt(&steady);
                    Ok(())
                }
            }
        });

        let records = supervisor.run().await;

        assert_eq!(steady.load(Ordering::SeqCst), 1, "it ran exactly once");
        assert_eq!(records[1].name, "steady");
        assert_eq!(records[1].stopped_by, Stopped::Finished);
        assert_eq!(records[1].restarts, 0);
    }
}
