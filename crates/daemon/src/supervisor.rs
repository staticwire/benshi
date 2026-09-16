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
//! This module reads the time from `tokio::time` rather than from an injected
//! `benshi_core::clock::Clock`, and it is the only place in the workspace that
//! does. It has to sleep on the same clock it measures with, and a `Clock`
//! cannot sleep, so taking one as a parameter would hand this module two clocks
//! free to disagree about whether a task had been running long enough. What the
//! rule is for still holds: `tokio::time` is virtualised under a paused runtime,
//! so `a_long_backoff_does_not_count_as_work` winds through 423 seconds of
//! retries and takes no measurable time to run.

use std::collections::HashMap;
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
/// before panicking starts the count again. A budget spent once and never
/// renewed would stop a task for the life of the process over a minute of
/// trouble it had long since recovered from.
pub const RESTART_LIMIT: u32 = 5;

/// How long to wait before the first retry of a transient failure.
pub const BACKOFF_BASE: Duration = Duration::from_secs(1);

/// The longest a retry is ever delayed.
///
/// A transient failure is something that is expected to pass: a bus that is
/// restarting, a network that is down. Doubling without a ceiling would leave a
/// task asleep for hours after an outage it has long recovered from.
pub const BACKOFF_CEILING: Duration = Duration::from_secs(60);

/// How long a task must run before a panic counts as new trouble rather than
/// the continuation of a crash loop.
///
/// [`RESTART_LIMIT`] counts consecutive failures, and consecutive has to mean
/// something in time. An attempt that ran this long did work, so the panic
/// that ends it starts a fresh count. Without that, six panics inside one
/// millisecond spend the entire budget - a panic is restarted with no delay -
/// and the task never runs again however long the process lives, which is the
/// opposite of what a restart limit is for.
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

/// One task's body, ready to be run again.
type TaskFuture = Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send>>;

/// A set of tasks whose handles are owned here and nowhere else.
#[derive(Default)]
pub struct Supervisor {
    tasks: Vec<Supervised>,
}

/// One task and what has happened to it so far.
struct Supervised {
    name: String,
    body: Box<dyn FnMut() -> TaskFuture + Send>,
    restarts: u32,
    panics: u32,
    /// When the current attempt was spawned, and how much of that it spent
    /// asleep before starting. The difference is how long it actually ran,
    /// which is what decides whether a panic continues a crash loop.
    last_start: Option<Instant>,
    last_delay: Duration,
    /// `None` for as long as it is still running.
    stopped_by: Option<Stopped>,
}

/// How long to wait before the `restarts`-th restart of a transient failure.
///
/// Doubling, from [`BACKOFF_BASE`] and never past [`BACKOFF_CEILING`]. Only a
/// transient failure is delayed: it is retried without limit, so it needs the
/// throttle. A panic is retried at most [`RESTART_LIMIT`] times, and a bounded
/// retry cannot run away.
fn backoff(restarts: u32) -> Duration {
    // `restarts` is unbounded for a transient failure, and an exponent large
    // enough to overflow would wrap to a short delay, turning the throttle into
    // its opposite. The clamp is what prevents that; the saturating forms only
    // keep it prevented if the constants above ever change.
    let doubling = 2_u32.saturating_pow(restarts.saturating_sub(1).min(16));

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
    /// A supervisor with nothing to supervise yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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
    pub async fn run(mut self) -> Vec<TaskRecord> {
        let mut set = JoinSet::new();
        let mut owners = HashMap::new();

        for (index, task) in self.tasks.iter_mut().enumerate() {
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
            let task = &mut self.tasks[index];

            match outcome {
                Ok(Ok(())) => task.stopped_by = Some(Stopped::Finished),
                Ok(Err(TaskError::Permanent(reason))) => {
                    task.stopped_by = Some(Stopped::Permanent(reason.to_string()));
                }
                Ok(Err(TaskError::Transient(_))) => {
                    task.restarts += 1;
                    let delay = backoff(task.restarts);
                    start(&mut set, &mut owners, task, index, delay);
                }
                Err(failure) if failure.is_panic() => {
                    let reason = panic_message(failure.into_panic());
                    // An attempt that ran longer than the progress interval got
                    // work done, so the panic that ended it starts a new count
                    // rather than continuing the last one. Absent a start time
                    // nothing has run, which counts as no progress.
                    let ran_for = task
                        .last_start
                        .map(|start| start.elapsed().saturating_sub(task.last_delay))
                        .unwrap_or_default();
                    if ran_for >= PROGRESS_INTERVAL {
                        task.panics = 0;
                    }

                    if task.panics >= RESTART_LIMIT {
                        task.stopped_by = Some(Stopped::Exhausted(reason));
                    } else {
                        task.panics += 1;
                        task.restarts += 1;
                        start(&mut set, &mut owners, task, index, Duration::ZERO);
                    }
                }
                // Nothing here aborts a task and the set outlives the loop, so
                // a cancellation means something else reached in and took the
                // handle. Recorded rather than guessed at.
                Err(_cancelled) => {
                    task.stopped_by = Some(Stopped::Permanent("the task was cancelled".to_owned()));
                }
            }
        }

        self.tasks
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
    use super::{PROGRESS_INTERVAL, RESTART_LIMIT, Stopped, Supervisor, TaskError, backoff};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Mutex, PoisonError};
    use tokio::time::Instant;

    /// A counter shared between a test and the task body it supervises.
    fn counter() -> Arc<AtomicU32> {
        Arc::new(AtomicU32::new(0))
    }

    /// Count this attempt and return which one it was, starting at one.
    fn attempt(count: &Arc<AtomicU32>) -> u32 {
        count.fetch_add(1, Ordering::SeqCst) + 1
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
        // The one way a crash loop could refund its own budget for ever: fail
        // transiently until the retry delay has grown to a minute, then panic
        // the instant the delay is over. Waiting is not working, so the attempt
        // is as short as it looks and the limit must still stop it.
        let ramp = (1..=32_u32)
            .find(|&restarts| backoff(restarts) >= PROGRESS_INTERVAL)
            .expect("the backoff ceiling reaches the progress interval");
        // Far past what a working limit needs, so a budget that does keep being
        // refunded still leaves this test finite.
        let escape = ramp + RESTART_LIMIT * 8;
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

                    // Transient until the delay reaches its ceiling, and then
                    // one transient between every pair of panics to hold it
                    // there: a panic on its own is restarted with no delay.
                    let panics_now = attempt > ramp && attempt.is_multiple_of(2);
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
