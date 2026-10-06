// Included verbatim by the doctests on `crate::background_events`, so this
// file names nothing from the crate and carries no inner attributes.

use windows_core::{InRef, Interface};

/// Wraps `f` as the handler of a `TypedEventHandler<S, A>` event that the
/// platform may raise on any thread — `MediaPlayer`, `MediaPlaybackSession`,
/// `MediaPlaybackItem`, `MediaPlaybackCommandManager` and `TimedMetadataTrack`
/// events all fire on background threads.
///
/// The generated event methods take the handler closure as is and only
/// require `'static`, so the bound lives here: `Send` because the closure
/// moves to whichever thread raises the event, `Sync` because the platform may
/// invoke one registration from several threads at once. `UI`-thread-only
/// state (`Rc`, `Cell`, `RefCell`, `Binding`) cannot be captured; such a
/// handler sends the values it reads to the `UI` thread instead.
pub(crate) fn background_handler<S, A>(
    f: impl Fn(Option<&S>, Option<&A>) + Send + Sync + 'static,
) -> impl Fn(InRef<'_, S>, InRef<'_, A>) + 'static
where
    S: Interface,
    A: Interface,
{
    move |sender, args| f(sender.as_ref(), args.as_ref())
}
