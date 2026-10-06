//! `DispatcherQueue`-backed executors.
//!
//! `WaterUI`'s `LocalExecutor` contract maps directly onto `WinUI`'s dispatcher:
//! `spawn_local` schedules runnables through `TryEnqueueWithPriority`, which is
//! the same queue `Application::Start` pumps on the UI thread.

use std::future::Future;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};

use executor_core::{
    LocalExecutor,
    async_task::{self, AsyncTask, Runnable},
};

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;

/// Schedules `f` on the `WinUI` dispatcher at normal priority, from any thread.
///
/// `f` crosses to the dispatcher's thread, so it must be `Send`; work that
/// captures UI-thread-only state goes through [`UiThread::enqueue`] instead.
pub fn enqueue_on_ui_thread(queue: &DispatcherQueue, f: impl FnOnce() + Send + 'static) {
    enqueue(queue, f);
}

/// The calling thread's `DispatcherQueue` as a capability to schedule
/// non-`Send` work back onto that same thread.
///
/// It is neither `Send` nor `Sync`, so it never leaves the thread it was
/// created on: every closure [`Self::enqueue`] accepts was built on the thread
/// that runs it. The runner creates it inside `Application::Start` and hands it
/// to the renderer, which passes it to the components that need it.
#[derive(Debug, Clone)]
pub struct UiThread {
    queue: DispatcherQueue,
    _thread_bound: PhantomData<Rc<()>>,
}

impl UiThread {
    /// Binds the calling thread's dispatcher.
    ///
    /// # Errors
    ///
    /// Returns an error when the calling thread has no `DispatcherQueue`.
    pub(crate) fn for_current_thread() -> windows_core::Result<Self> {
        Ok(Self {
            queue: DispatcherQueue::GetForCurrentThread()?,
            _thread_bound: PhantomData,
        })
    }

    /// Schedules `f` on this thread's dispatcher at normal priority.
    pub fn enqueue(&self, f: impl FnOnce() + 'static) {
        enqueue(&self.queue, f);
    }

    /// The underlying dispatcher, for handing to code that schedules `Send`
    /// work from other threads.
    pub const fn queue(&self) -> &DispatcherQueue {
        &self.queue
    }
}

/// The unchecked enqueue both entry points share: the generated
/// `DispatcherQueueHandler::new` carries no `Send` bound, so the callers
/// establish that `f` may run on the dispatcher's thread.
fn enqueue(queue: &DispatcherQueue, f: impl FnOnce() + 'static) {
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
    /// Posts to the dispatcher of `ui`'s thread.
    pub fn new(ui: &UiThread) -> Self {
        Self {
            queue: ui.queue().clone(),
        }
    }

    /// The underlying `WinUI` dispatcher.
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
