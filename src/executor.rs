//! `DispatcherQueue`-backed executors.
//!
//! `WaterUI`'s `LocalExecutor` contract maps directly onto `WinUI`'s dispatcher:
//! `spawn_local` schedules runnables through `TryEnqueueWithPriority`, which is
//! the same queue `Application::Start` pumps on the UI thread.

use std::any::Any;
use std::future::Future;
use std::panic::{self, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};

use executor_core::{
    LocalExecutor,
    async_task::{AsyncTask, Runnable},
};

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;

/// Schedules `f` on the `WinUI` dispatcher at normal priority.
///
/// The callback itself runs on the UI thread; it may capture non-`Send` state
/// because `DispatcherQueueHandler` is a synchronous delegate invoked in place.
///
/// A panic in `f` is contained and logged here rather than unwinding further:
/// the handler is called through an `extern "system"` vtable slot, and an
/// unwind that reaches it aborts the process with "panic in a function that
/// cannot unwind", burying the original panic (#56).
pub fn enqueue_on_ui_thread(queue: &DispatcherQueue, f: impl FnOnce() + 'static) {
    // `DispatcherQueueHandler` requires `Fn`, but an enqueued task is invoked
    // exactly once, so the closure is wrapped for a single take.
    let f = std::cell::RefCell::new(Some(f));
    queue
        .TryEnqueueWithPriority(
            DispatcherQueuePriority::Normal,
            &DispatcherQueueHandler::new(move || {
                let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                    f.borrow_mut()
                        .take()
                        .expect("dispatcher callback ran twice")();
                }));
                if let Err(payload) = outcome {
                    tracing::error!(
                        panic = panic_message(payload.as_ref()),
                        "a dispatcher callback panicked; contained at the WinUI boundary",
                    );
                }
            }),
        )
        .expect("DispatcherQueue::TryEnqueue failed");
}

/// Spawns a UI-thread task whose panics stay inside the task.
///
/// With `propagate_panic`, a panic while polling `fut` is stored as the task's
/// output: `Runnable::run` returns normally on the dispatcher, and the original
/// payload is re-raised wherever the task is awaited (or returned as `Err` from
/// `AsyncTask::result`). Without it, the unwind leaves `Runnable::run` and the
/// awaiter only ever sees "Task polled after completion".
fn spawn_ui_task<Fut, S>(fut: Fut, schedule: S) -> (Runnable, AsyncTask<Fut::Output>)
where
    Fut: Future + 'static,
    Fut::Output: 'static,
    S: Fn(Runnable) + Send + Sync + 'static,
{
    let fut = ReportPanics(Box::pin(fut));
    let (runnable, task) = async_task::Builder::new()
        .propagate_panic(true)
        .spawn_local(move |()| fut, schedule);
    (runnable, AsyncTask::from(task))
}

/// Logs a panic raised while polling the wrapped future, then lets it continue
/// unwinding into `async-task`, which stores it for the awaiter.
///
/// Without this, a panic in a detached task would be dropped with the task and
/// leave no trace in `tracing` output.
struct ReportPanics<F: Future>(Pin<Box<F>>);

impl<F: Future> Future for ReportPanics<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = self.0.as_mut();
        match panic::catch_unwind(AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(poll) => poll,
            Err(payload) => {
                tracing::error!(
                    panic = panic_message(payload.as_ref()),
                    "a UI-thread task panicked",
                );
                panic::resume_unwind(payload)
            }
        }
    }
}

/// The message of a `panic!` payload, which is a `&str` or a `String` for every
/// panic raised through the standard macros.
fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("<non-string panic payload>")
}

/// `LocalExecutor` implementation posting work to a `DispatcherQueue`.
#[derive(Debug, Clone)]
pub struct DispatcherQueueExecutor {
    queue: DispatcherQueue,
}

impl DispatcherQueueExecutor {
    /// Wraps the dispatcher of the calling (UI) thread.
    pub fn for_current_thread() -> windows_core::Result<Self> {
        Ok(Self {
            queue: DispatcherQueue::GetForCurrentThread()?,
        })
    }

    /// The underlying `WinUI` dispatcher, for watcher callbacks that must hop
    /// back to the UI thread.
    pub const fn queue(&self) -> &DispatcherQueue {
        &self.queue
    }
}

impl LocalExecutor for DispatcherQueueExecutor {
    type Task<T: 'static> = AsyncTask<T>;

    fn spawn_local<Fut>(&self, fut: Fut) -> Self::Task<Fut::Output>
    where
        Fut: Future + 'static,
    {
        let queue = self.queue.clone();
        let (runnable, task) = spawn_ui_task(fut, move |runnable: Runnable| {
            enqueue_on_ui_thread(&queue, move || {
                runnable.run();
            });
        });
        runnable.schedule();
        task
    }
}

/// `LocalExecutor` for the window between STA initialization and
/// `Application::Start`.
///
/// `Application::Start` owns creation of the UI thread's `DispatcherQueue`, so
/// a backend cannot post to it before `Start` runs — yet `app(env)` factories
/// such as `waterui_browser_cef::install` already `spawn_local` while the
/// `App` is being built. This executor buffers those schedules on the calling
/// thread and flushes them onto the dispatcher once [`Self::activate`] runs
/// inside the `Start` callback, which is when the queue exists and the message
/// loop is about to pump.
#[derive(Debug, Clone)]
pub struct DeferredDispatcherExecutor {
    queue: Arc<OnceLock<DispatcherQueue>>,
    pending: Arc<Mutex<Vec<Runnable>>>,
}

impl Default for DeferredDispatcherExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl DeferredDispatcherExecutor {
    /// Creates a deactivated executor that buffers scheduled runnables.
    pub fn new() -> Self {
        Self {
            queue: Arc::new(OnceLock::new()),
            pending: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Binds the calling thread's `DispatcherQueue` and replays every buffered
    /// schedule onto it. Must run on the thread the executor was installed on —
    /// inside `Application::Start`'s callback, after the queue exists.
    pub fn activate(&self) -> windows_core::Result<()> {
        let queue = DispatcherQueue::GetForCurrentThread()?;
        self.queue
            .set(queue.clone())
            .expect("DeferredDispatcherExecutor activated twice");
        for runnable in self
            .pending
            .lock()
            .expect("pending schedule buffer poisoned")
            .drain(..)
        {
            enqueue_on_ui_thread(&queue, move || {
                runnable.run();
            });
        }
        Ok(())
    }
}

impl LocalExecutor for DeferredDispatcherExecutor {
    type Task<T: 'static> = AsyncTask<T>;

    fn spawn_local<Fut>(&self, fut: Fut) -> Self::Task<Fut::Output>
    where
        Fut: Future + 'static,
    {
        let queue = self.queue.clone();
        let pending = self.pending.clone();
        let (runnable, task) = spawn_ui_task(fut, move |runnable: Runnable| {
            if let Some(queue) = queue.get() {
                enqueue_on_ui_thread(queue, move || {
                    runnable.run();
                });
            } else {
                pending
                    .lock()
                    .expect("pending schedule buffer poisoned")
                    .push(runnable);
            }
        });
        runnable.schedule();
        task
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::task::Waker;

    use super::*;

    fn panics() -> impl Future<Output = ()> {
        std::future::poll_fn(|_| -> Poll<()> { panic!("boom from {}", "a task") })
    }

    /// A panicking task must not unwind out of `Runnable::run` — on the
    /// dispatcher that unwind would cross the `extern "system"` handler — and
    /// its awaiter must see the original payload, not "Task polled after
    /// completion".
    #[test]
    fn task_panic_stays_in_the_task_and_reaches_the_awaiter() {
        let (sender, _queue) = mpsc::channel::<Runnable>();
        let (runnable, task) = spawn_ui_task(panics(), move |runnable| {
            sender.send(runnable).expect("test queue is open");
        });

        let run = panic::catch_unwind(AssertUnwindSafe(|| runnable.run()));
        assert!(run.is_ok(), "the task's panic unwound out of Runnable::run");

        let mut result = std::pin::pin!(task.result());
        let mut cx = Context::from_waker(Waker::noop());
        let Poll::Ready(Err(payload)) = result.as_mut().poll(&mut cx) else {
            panic!("awaiting a panicked task should yield its panic");
        };
        assert_eq!(panic_message(payload.as_ref()), "boom from a task");
    }

    #[test]
    fn panic_message_reads_static_and_formatted_payloads() {
        let literal = panic::catch_unwind(|| {
            panic!("literal");
        })
        .unwrap_err();
        let formatted = panic::catch_unwind(|| {
            panic!("{}", "formatted");
        })
        .unwrap_err();
        let other = panic::catch_unwind(|| {
            panic::panic_any(7_u8);
        })
        .unwrap_err();

        assert_eq!(panic_message(literal.as_ref()), "literal");
        assert_eq!(panic_message(formatted.as_ref()), "formatted");
        assert_eq!(panic_message(other.as_ref()), "<non-string panic payload>");
    }
}
