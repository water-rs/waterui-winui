//! Logoff, shutdown and console interrupts → the app's `Termination` machine.
//!
//! Mirrors Hydrolysis' winit runner: each application window's `HWND` is
//! subclassed so `WM_QUERYENDSESSION` files a cancellable request (holding
//! the shutdown with a `ShutdownBlockReason` while the hooks run) and
//! `WM_ENDSESSION` a required one, Ctrl+C / Ctrl+Break file a required
//! request through `ctrlc`, and closing the console files one and holds
//! the close until `on_terminate` finished.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::mpsc;

use executor_core::{LocalExecutor, Task};
use waterui::app::{TerminationHandle, TerminationKind};

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;
use crate::executor::DispatcherQueueExecutor;

/// What the session-end paths share with the runner's `TerminationHost`.
///
/// Windows ends the process as soon as a session-end notification returns
/// — `WM_ENDSESSION` on an application window, or the console close event
/// — so each holds its notification open until the machine reports
/// `terminate`, which is what proves `on_terminate` ran to completion.
#[derive(Default)]
pub(crate) struct SessionEndState {
    /// Windows holding a `ShutdownBlockReasonCreate`, filled by the
    /// `WM_QUERYENDSESSION` veto.
    block_reasons: RefCell<HashSet<HWND>>,
    /// Whether the machine reported `terminate`.
    terminated: Cell<bool>,
    /// Console close handlers waiting for `terminate`, each released by a
    /// send on its channel.
    console_waiters: RefCell<Vec<mpsc::Sender<()>>>,
}

impl SessionEndState {
    /// Destroys every outstanding `ShutdownBlockReason` — on `terminate`
    /// because the veto is moot, on `refuse` because it is lifted.
    ///
    /// # Panics
    ///
    /// When `ShutdownBlockReasonDestroy` fails on a live window.
    pub(crate) fn destroy_block_reasons(&self) {
        for hwnd in self.block_reasons.borrow_mut().drain() {
            // SAFETY: `hwnd` is a live application window — a destroyed one
            // left the set in `session_end_proc`'s `WM_NCDESTROY` arm.
            let destroyed = unsafe { ShutdownBlockReasonDestroy(hwnd) };
            assert!(
                destroyed.as_bool(),
                "ShutdownBlockReasonDestroy failed: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    /// The machine reported `terminate`: release every session end waiting
    /// on it.
    pub(crate) fn terminated(&self) {
        self.destroy_block_reasons();
        self.terminated.set(true);
        for waiter in self.console_waiters.borrow_mut().drain(..) {
            // A waiter whose handler thread is gone has nothing to release.
            let _ = waiter.send(());
        }
    }

    /// Holds a console close handler until the machine reports
    /// `terminate` — released at once when it already has.
    fn release_console_on_terminate(&self, waiter: mpsc::Sender<()>) {
        if self.terminated.get() {
            let _ = waiter.send(());
        } else {
            self.console_waiters.borrow_mut().push(waiter);
        }
    }

    /// Pumps the thread's messages — the `DispatcherQueue` the local
    /// executor posts to among them — until the machine reports
    /// `terminate`.
    ///
    /// `WM_ENDSESSION` is dispatched from inside the `WinUI` message loop,
    /// which pumps nothing until the message returns, so `on_terminate`
    /// would never be polled without this loop.
    ///
    /// # Panics
    ///
    /// When `MsgWaitForMultipleObjectsEx` fails.
    fn run_until_terminated(&self) {
        while !self.terminated.get() {
            // SAFETY: no handles are passed, so the wait is on this thread's
            // message queue alone.
            let woke = unsafe {
                MsgWaitForMultipleObjectsEx(
                    0,
                    core::ptr::null(),
                    INFINITE,
                    QS_ALLINPUT.cast_unsigned(),
                    MWMO_INPUTAVAILABLE.cast_unsigned(),
                )
            };
            assert!(
                woke != WAIT_FAILED,
                "waiting for on_terminate at session end failed: {}",
                std::io::Error::last_os_error()
            );
            let mut msg = MSG::default();
            // SAFETY: `msg` is a writable `MSG`; a null `HWND` takes every
            // message of this thread.
            while unsafe {
                PeekMessageW(
                    &raw mut msg,
                    core::ptr::null_mut(),
                    0,
                    0,
                    PM_REMOVE.cast_unsigned(),
                )
            }
            .as_bool()
            {
                if msg.message == WM_QUIT.cast_unsigned() {
                    // `Application::Exit` posted this quit to end the outer
                    // `WinUI` loop; swallowing it here would strand the
                    // dispatcher after `WM_ENDSESSION` unwinds. Repost it —
                    // `PeekMessageW` already removed it — and let the outer
                    // loop take it.
                    // SAFETY: posts WM_QUIT to this thread's own queue; no pointers are involved.
                    unsafe {
                        PostQuitMessage(
                            i32::try_from(msg.wParam.cast_signed())
                                .expect("WM_QUIT carries the i32 PostQuitMessage was given"),
                        )
                    };
                    return;
                }
                // SAFETY: `msg` was just filled by `PeekMessageW`.
                unsafe {
                    // The BOOL only says whether a character message was
                    // posted.
                    let _ = TranslateMessage(&raw const msg);
                    DispatchMessageW(&raw const msg);
                }
            }
        }
    }
}

/// The machine and the shared session-end state, handed to every
/// application window's subclass.
#[derive(Clone)]
pub(crate) struct SessionEnd {
    termination: TerminationHandle,
    state: Rc<SessionEndState>,
}

/// `SetWindowSubclass` id for the session-end subclass; arbitrary but unique
/// per window.
const SESSION_END_SUBCLASS_ID: usize = 0x5755_5345; // "WUSE"

impl SessionEnd {
    pub(crate) const fn new(termination: TerminationHandle, state: Rc<SessionEndState>) -> Self {
        Self { termination, state }
    }

    /// Subclasses an application window's `HWND` for the session-end
    /// messages; the subclass removes itself on `WM_NCDESTROY`.
    ///
    /// # Panics
    ///
    /// When `SetWindowSubclass` fails: the window's session end would skip
    /// `on_terminate`.
    pub(crate) fn install(self, hwnd: HWND) {
        let subclass = Box::into_raw(Box::new(self));
        // SAFETY: `hwnd` is a live application window; the box is reclaimed
        // by `session_end_proc` on `WM_NCDESTROY`.
        let installed = unsafe {
            SetWindowSubclass(
                hwnd,
                Some(session_end_proc),
                SESSION_END_SUBCLASS_ID,
                subclass as usize,
            )
        };
        if !installed.as_bool() {
            // SAFETY: the subclass was not installed, so nothing else holds
            // the allocation.
            drop(unsafe { Box::from_raw(subclass) });
            panic!(
                "SetWindowSubclass failed on an application window, so its session end would \
                 skip on_terminate"
            );
        }
    }
}

/// How the session-end subclass answers a window message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionEndAnswer {
    /// Not a session end the machine hears about: the previous proc answers.
    Forward,
    /// `WM_QUERYENDSESSION` with a hook to ask: block the shutdown with a
    /// reason, answer `FALSE`, and file a cancellable request — the machine
    /// drops the per-window repetition.
    Veto,
    /// `WM_ENDSESSION` with `wParam` `TRUE`: the session ends whatever the
    /// app answered, so file a required request and hold the message until
    /// `on_terminate` finished.
    End,
}

impl SessionEndAnswer {
    /// The answer for `msg`. A query with no hook passes through, letting
    /// the session end, since nothing could refuse it.
    const fn of(msg: u32, wparam: WPARAM, has_hooks: bool) -> Self {
        if msg == WM_QUERYENDSESSION.cast_unsigned() && has_hooks {
            Self::Veto
        } else if msg == WM_ENDSESSION.cast_unsigned() && wparam != 0 {
            Self::End
        } else {
            Self::Forward
        }
    }

    /// The request this answer files with the machine.
    const fn request(self) -> Option<TerminationKind> {
        match self {
            Self::Forward => None,
            Self::Veto => Some(TerminationKind::Cancellable),
            Self::End => Some(TerminationKind::Required),
        }
    }
}

unsafe extern "system" fn session_end_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    uidsubclass: usize,
    refdata: usize,
) -> LRESULT {
    if msg == WM_NCDESTROY.cast_unsigned() {
        // SAFETY: `refdata` is the box `SessionEnd::install` leaked; the
        // subclass is removed before the box is reclaimed, so no later
        // message sees it.
        unsafe {
            let _ = RemoveWindowSubclass(hwnd, Some(session_end_proc), uidsubclass);
            let subclass = Box::from_raw(refdata as *mut SessionEnd);
            // A reason this window still holds dies with it.
            subclass.state.block_reasons.borrow_mut().remove(&hwnd);
        }
        // SAFETY: forwarding to the previous proc.
        return unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: `refdata` is the `SessionEnd` box installed by
    // `SessionEnd::install`, alive until `WM_NCDESTROY` above. The handles
    // are cloned out at once: `request` can run `on_terminate` — an
    // `Application::Exit` or a window closing inside the nested pump
    // destroys this window, and `WM_NCDESTROY` frees the box.
    let SessionEnd { termination, state } = unsafe { &*(refdata as *const SessionEnd) }.clone();
    let answer = SessionEndAnswer::of(msg, wparam, termination.has_hooks());
    let Some(request) = answer.request() else {
        // SAFETY: forwarding every unclaimed message to the previous proc.
        return unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
    };
    if answer == SessionEndAnswer::Veto {
        // SAFETY: `hwnd` is the live window the message arrived on.
        let blocked = unsafe {
            ShutdownBlockReasonCreate(hwnd, windows_core::w!("The application is finishing up"))
        };
        assert!(
            blocked.as_bool(),
            "ShutdownBlockReasonCreate failed: {}",
            std::io::Error::last_os_error()
        );
        state.block_reasons.borrow_mut().insert(hwnd);
    }
    termination.request(request);
    if answer == SessionEndAnswer::End {
        state.run_until_terminated();
    }
    // `FALSE` to the query (the veto); the processed-message result to
    // `WM_ENDSESSION`.
    0
}

/// What a console interrupt does, given whether one arrived before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterruptAction {
    /// File a required request, the way the last window closing does.
    Request,
    /// Stop asking: the app had its chance and did not take it.
    ForceExit,
}

/// Collapses repeated interrupts into "ask once, then stop asking", so a
/// wedged UI thread stays killable from the console that started it.
#[derive(Debug, Default)]
struct Interrupts {
    requested: bool,
}

impl Interrupts {
    const fn record(&mut self) -> InterruptAction {
        if self.requested {
            InterruptAction::ForceExit
        } else {
            self.requested = true;
            InterruptAction::Request
        }
    }
}

/// Shell convention for a process killed by SIGINT (128 + 2), which is what
/// a user pressing Ctrl+C a second time asked for.
const FORCED_INTERRUPT_EXIT_CODE: i32 = 130;

/// Turns Ctrl+C, Ctrl+Break and closing the console into a required
/// request.
///
/// `ctrlc` runs its handler on a thread of its own, so the request crosses
/// to the UI thread over a channel, where the task holding `termination`
/// files it. A second interrupt exits at once. Closing the console goes to
/// [`console_close_handler`], registered after `ctrlc`'s so it runs first.
///
/// # Panics
///
/// When a console handler was already installed in this process.
pub(crate) fn install_console_interrupt(
    executor: &DispatcherQueueExecutor,
    termination: TerminationHandle,
    state: Rc<SessionEndState>,
) {
    let (sender, receiver) = async_channel::bounded::<()>(1);
    let mut interrupts = Interrupts::default();
    ctrlc::set_handler(move || match interrupts.record() {
        InterruptAction::Request => {
            // The receiver is gone only once the dispatcher has shut down,
            // when there is nothing left to terminate.
            let _ = sender.try_send(());
        }
        InterruptAction::ForceExit => {
            tracing::warn!("console interrupt repeated, exiting without running on_terminate");
            std::process::exit(FORCED_INTERRUPT_EXIT_CODE);
        }
    })
    .expect("installing the console interrupt handler failed");
    let interrupted = termination.clone();
    executor
        .spawn_local(async move {
            if receiver.recv().await.is_ok() {
                interrupted.request(TerminationKind::Required);
            }
        })
        .detach();
    install_console_close(executor, termination, state);
}

/// How the console control handler reaches the UI thread: each close event
/// sends the channel its handler thread waits on.
///
/// `SetConsoleCtrlHandler` calls a bare function pointer with no context
/// argument, so the one piece of state the handler needs — how to reach the
/// UI thread — has to live in a process-wide slot. It is set once, by
/// [`install_console_close`].
static CONSOLE_CLOSE: std::sync::OnceLock<async_channel::Sender<mpsc::Sender<()>>> =
    std::sync::OnceLock::new();

/// Holds the console close event open until `on_terminate` finished.
///
/// Windows ends the process as soon as the handler for this event returns,
/// and `ctrlc`'s handler returns at once, so the request it would file
/// could never be served. This handler runs first — the system calls the
/// most recently registered handler first — files a required request on the
/// UI thread, and blocks its own thread, which the system created for the
/// event, until the machine reports `terminate` or the system's grace period
/// ends the process. Ctrl+C and Ctrl+Break fall through to `ctrlc`. The
/// logoff and shutdown control events never arrive: the system withholds
/// them from a process that loads user32, and the application windows
/// receive `WM_ENDSESSION` instead.
unsafe extern "system" fn console_close_handler(ctrl_type: u32) -> windows_core::BOOL {
    if ctrl_type != CTRL_CLOSE_EVENT.cast_unsigned() {
        return windows_core::BOOL::from(false);
    }
    let close = CONSOLE_CLOSE
        .get()
        .expect("the console handler is registered after its channel is recorded");
    let (released, release) = mpsc::channel();
    // A dispatcher that already shut down dropped the receiver, and with it
    // `released`, which ends the wait at once.
    let _ = close.send_blocking(released);
    let _ = release.recv();
    windows_core::BOOL::from(true)
}

/// Registers [`console_close_handler`] ahead of `ctrlc`'s handler, and the
/// UI-thread task that files its requests.
///
/// # Panics
///
/// When it is called a second time in the process, or when
/// `SetConsoleCtrlHandler` fails.
fn install_console_close(
    executor: &DispatcherQueueExecutor,
    termination: TerminationHandle,
    state: Rc<SessionEndState>,
) {
    let (sender, receiver) = async_channel::unbounded::<mpsc::Sender<()>>();
    assert!(
        CONSOLE_CLOSE.set(sender).is_ok(),
        "the console close handler is installed once per process"
    );
    // SAFETY: `console_close_handler` matches `PHANDLER_ROUTINE` and lives
    // for the whole process.
    let installed = unsafe {
        SetConsoleCtrlHandler(Some(console_close_handler), windows_core::BOOL::from(true))
    };
    assert!(
        installed.as_bool(),
        "failed to install the console close handler: {}",
        std::io::Error::last_os_error()
    );
    executor
        .spawn_local(async move {
            while let Ok(waiter) = receiver.recv().await {
                state.release_console_on_terminate(waiter);
                termination.request(TerminationKind::Required);
            }
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, InterruptAction, Interrupts, SessionEndAnswer,
        SessionEndState, WM_CLOSE, WM_ENDSESSION, WM_QUERYENDSESSION, console_close_handler,
    };
    use waterui::app::TerminationKind;

    #[test]
    fn query_end_session_files_a_cancellable_request_only_with_hooks() {
        let query = WM_QUERYENDSESSION.cast_unsigned();
        let with_hooks = SessionEndAnswer::of(query, 0, true);
        assert_eq!(with_hooks, SessionEndAnswer::Veto);
        assert_eq!(with_hooks.request(), Some(TerminationKind::Cancellable));

        let without_hooks = SessionEndAnswer::of(query, 0, false);
        assert_eq!(without_hooks, SessionEndAnswer::Forward);
        assert_eq!(without_hooks.request(), None);
    }

    #[test]
    fn end_session_files_a_required_request_only_when_the_session_ends() {
        let end = WM_ENDSESSION.cast_unsigned();
        for has_hooks in [true, false] {
            let ending = SessionEndAnswer::of(end, 1, has_hooks);
            assert_eq!(ending, SessionEndAnswer::End);
            assert_eq!(ending.request(), Some(TerminationKind::Required));

            let cancelled = SessionEndAnswer::of(end, 0, has_hooks);
            assert_eq!(cancelled, SessionEndAnswer::Forward);
            assert_eq!(cancelled.request(), None);
        }
    }

    #[test]
    fn other_messages_pass_through() {
        assert_eq!(
            SessionEndAnswer::of(WM_CLOSE.cast_unsigned(), 1, true),
            SessionEndAnswer::Forward
        );
    }

    #[test]
    fn first_interrupt_requests_and_later_ones_force_the_exit() {
        let mut interrupts = Interrupts::default();
        assert_eq!(interrupts.record(), InterruptAction::Request);
        assert_eq!(interrupts.record(), InterruptAction::ForceExit);
        assert_eq!(interrupts.record(), InterruptAction::ForceExit);
    }

    #[test]
    fn console_close_handler_leaves_interrupts_to_ctrlc() {
        for event in [CTRL_C_EVENT, CTRL_BREAK_EVENT] {
            // SAFETY: a non-close event returns before touching any state.
            let handled = unsafe { console_close_handler(event.cast_unsigned()) };
            assert!(!handled.as_bool());
        }
    }

    #[test]
    fn console_close_waits_until_the_machine_terminates() {
        let state = SessionEndState::default();
        let (waiting, released) = mpsc::channel();
        state.release_console_on_terminate(waiting);
        assert_eq!(released.try_recv(), Err(mpsc::TryRecvError::Empty));
        state.terminated();
        assert_eq!(released.try_recv(), Ok(()));

        let (late, released_late) = mpsc::channel();
        state.release_console_on_terminate(late);
        assert_eq!(released_late.try_recv(), Ok(()));
    }
}
