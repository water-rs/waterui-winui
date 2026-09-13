//! `DispatcherQueue`-backed executors.
//!
//! WaterUI's `LocalExecutor` contract maps directly onto WinUI's dispatcher:
//! `spawn_local` schedules runnables through `TryEnqueueWithPriority`, which is
//! the same queue `Application::Start` pumps on the UI thread.

use std::future::Future;

use executor_core::{
    LocalExecutor,
    async_task::{self, AsyncTask, Runnable},
};

use crate::bindings::*;

/// Schedules `f` on the WinUI dispatcher at normal priority.
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
                    .expect("dispatcher callback ran twice")()
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

    /// The underlying WinUI dispatcher, for watcher callbacks that must hop
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
