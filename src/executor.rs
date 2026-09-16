//! `DispatcherQueue`-backed executors.
//!
//! `WaterUI`'s `LocalExecutor` contract maps directly onto `WinUI`'s dispatcher:
//! `spawn_local` schedules runnables through `TryEnqueueWithPriority`, which is
//! the same queue `Application::Start` pumps on the UI thread.

use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};

use executor_core::{
    LocalExecutor,
    async_task::{self, AsyncTask, Runnable},
};

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;

/// Schedules `f` on the `WinUI` dispatcher at normal priority.
///
/// The callback itself runs on the UI thread; it may capture non-`Send` state
/// because `DispatcherQueueHandler` is a synchronous delegate invoked in place.
pub fn enqueue_on_ui_thread(queue: &DispatcherQueue, f: impl FnOnce() + 'static) {
    // `DispatcherQueueHandler` requires `Fn`, but an enqueued task is invoked
    // exactly once, so the closure is wrapped for a single take.
    let f = std::cell::RefCell::new(Some(f));
    queue
        .TryEnqueueWithPriority(
            DispatcherQueuePriority::Normal,
            &DispatcherQueueHandler::new(move || {
                f.borrow_mut()
                    .take()
                    .expect("dispatcher callback ran twice")();
            }),
        )
        .expect("DispatcherQueue::TryEnqueue failed");
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
        let (runnable, task) = async_task::spawn_local(fut, move |runnable: Runnable| {
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
        let (runnable, task) = async_task::spawn_local(fut, move |runnable: Runnable| {
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
