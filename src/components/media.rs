//! `Video` and `VideoPlayer` — native playback over `Windows.Media.Playback`.
//!
//! Both components map onto `MediaPlayerElement` driven by a `MediaPlayer`;
//! `VideoPlayer` additionally enables the transport controls. The shared
//! `PlaybackConfiguration` contract is honored exactly: the coordinator owns
//! the `MediaPlayer`, watches the command bindings (`source`,
//! `desired_playing`, seek/step generations, `volume`, `muted`,
//! `playback_rate`, track selections, `repeat`, `shuffle`, `has_next` /
//! `has_previous`) and writes the observed bindings (`position_seconds`,
//! `duration_seconds`, `phase`, `track_catalog`, `live_window`).
//!
//! Deliberately unmapped contract surface, because `MediaPlayer` exposes no
//! control for it: `preserve_pitch` (the pipeline always pitch-preserves),
//! `DrmConfiguration` fields beyond what the platform CDM negotiates on its
//! own, spherical `VideoProjection`, and `PlaybackPowerPolicy` requirements —
//! a required power path fails preparation rather than falling back silently.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use executor_core::LocalExecutor;
use nami::Signal;
use nami::watcher::BoxWatcherGuard;
use waterui_core::{Environment, Native};
use waterui_video::source::{Delivery, MediaItem};
use waterui_video::video::{
    AudioTrackInfo, AudioTrackSelection, BoundVideoEventHandler, ContentMode, Event, LiveWindow,
    NativeVideoConfig, NativeVideoPlayerConfig, PlaybackConfiguration, PlaybackOutputPath,
    PlaybackPowerPolicy, SubtitleSelection, SubtitleTrackInfo, SubtitleTrackOrigin, TimedMetadata,
    TrackCatalog, VideoProjection, VideoTrackInfo, VideoTrackSelection,
};
use waterui_video::{PlaybackPhase, PlayerController, RepeatMode};
use windows_core::Interface;

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;
use crate::component::WinUiComponent;
use crate::executor::{DispatcherQueueExecutor, enqueue_on_ui_thread};
use crate::renderer::WinUiRenderer;
use crate::util::{
    framework, store_event_revoker, store_retained, store_watcher_guards, subscribe_then_get,
};

/// 100ns ticks per second — the `TimeSpan` quantum.
const TICKS_PER_SECOND: f64 = 10_000_000.0;

fn timespan_seconds(value: windows_time::TimeSpan) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let seconds = value.duration as f64 / TICKS_PER_SECOND;
    seconds
}

fn seconds_timespan(seconds: f64) -> windows_time::TimeSpan {
    #[allow(clippy::cast_possible_truncation)]
    let ticks = (seconds * TICKS_PER_SECOND) as i64;
    windows_time::TimeSpan { duration: ticks }
}

/// Maps a `WaterUI` content mode onto the `MediaPlayerElement` stretch.
const fn content_mode_stretch(mode: ContentMode) -> Stretch {
    match mode {
        ContentMode::Fit => Stretch::Uniform,
        ContentMode::Fill => Stretch::UniformToFill,
        ContentMode::Stretch => Stretch::Fill,
    }
}

/// Non-empty track label — catalog descriptors reject blank labels.
fn track_label(track: &impl MediaTrackLabel, role: &str, index: u32) -> String {
    let label = track.label();
    if label.is_empty() {
        format!("{role} {}", index + 1)
    } else {
        label
    }
}

/// Shared label access over `IMediaTrack`-implementing track objects.
trait MediaTrackLabel {
    fn label(&self) -> String;
    fn language(&self) -> Option<String>;
}

impl MediaTrackLabel for AudioTrack {
    fn label(&self) -> String {
        self.Label().unwrap_or_default()
    }
    fn language(&self) -> Option<String> {
        self.Language().ok().filter(|s| !s.is_empty())
    }
}

impl MediaTrackLabel for VideoTrack {
    fn label(&self) -> String {
        self.Label().unwrap_or_default()
    }
    fn language(&self) -> Option<String> {
        self.Language().ok().filter(|s| !s.is_empty())
    }
}

impl MediaTrackLabel for TimedMetadataTrack {
    fn label(&self) -> String {
        self.cast::<IMediaTrack>()
            .and_then(|t| t.Label())
            .unwrap_or_default()
    }
    fn language(&self) -> Option<String> {
        self.cast::<IMediaTrack>()
            .ok()
            .and_then(|t| t.Language().ok())
            .filter(|s| !s.is_empty())
    }
}

/// The native side of one rendered `Video`/`VideoPlayer`.
///
/// Retained by the element's `Tag`; every `WinRT` event handler and nami
/// watcher captures only a `Weak`, so dropping the element releases the whole
/// coordinator (and the `MediaPlayer` it owns) without a reference cycle.
struct VideoCoordinator {
    player: MediaPlayer,
    /// `IMediaPlayer3` — session, command manager, realtime flag, frame steps.
    player3: IMediaPlayer3,
    session: MediaPlaybackSession,
    controller: PlayerController,
    on_event: Option<BoundVideoEventHandler>,
    executor: DispatcherQueueExecutor,
    /// Initial adaptive bitrate in bits per second (from the network policy).
    initial_bitrate: u32,
    /// The item currently set as the player's source.
    item: RefCell<Option<MediaPlaybackItem>>,
    /// `WinRT` track-list subscriptions on `item` — swapped per source.
    item_revokers: RefCell<Vec<windows_core::EventRevoker>>,
    /// `CueEntered` subscriptions — re-armed whenever the track list mutates.
    cue_revokers: RefCell<Vec<windows_core::EventRevoker>>,
    /// `Video::loops` folded into the looping policy.
    loops: bool,
    /// `true` while the active source is a live adaptive stream.
    live: Cell<bool>,
    /// Monotonic load counter — stale async source creations drop out.
    source_generation: Cell<u64>,

    source: waterui_core::Computed<MediaItem>,
    desired_playing: waterui_core::Binding<bool>,
    seek_target_seconds: waterui_core::Binding<f64>,
    seek_generation: waterui_core::Binding<u64>,
    step_forward_generation: waterui_core::Binding<u64>,
    step_backward_generation: waterui_core::Binding<u64>,
    volume: waterui_core::Binding<waterui_video::video::Volume>,
    muted: waterui_core::Binding<bool>,
    playback_rate: waterui_core::Binding<f32>,
    repeat: waterui_core::Binding<RepeatMode>,
    shuffle: waterui_core::Binding<bool>,
    has_next: waterui_core::Binding<bool>,
    has_previous: waterui_core::Binding<bool>,
    audio_track_selection: waterui_core::Binding<AudioTrackSelection>,
    video_track_selection: waterui_core::Binding<VideoTrackSelection>,
    subtitle_selection: waterui_core::Binding<SubtitleSelection>,

    position_seconds: waterui_core::Binding<f64>,
    duration_seconds: waterui_core::Binding<f64>,
    phase: waterui_core::Binding<PlaybackPhase>,
    track_catalog: waterui_core::Binding<TrackCatalog>,
    live_window: waterui_core::Binding<Option<LiveWindow>>,
}

impl VideoCoordinator {
    fn emit(&self, event: Event) {
        if let Some(handler) = &self.on_event {
            handler.call(event);
        }
    }

    /// Maps the session playback state onto the shared phase binding, the
    /// system transport controls, and `PlaybackStateChanged` events.
    fn on_session_state(&self, state: MediaPlaybackState) {
        let phase = match state {
            MediaPlaybackState::Opening => PlaybackPhase::Preparing,
            MediaPlaybackState::Buffering => PlaybackPhase::Buffering,
            MediaPlaybackState::Playing => PlaybackPhase::Playing,
            MediaPlaybackState::Paused => PlaybackPhase::Paused,
            _ => PlaybackPhase::Idle,
        };
        self.phase.set(phase);
        if let Ok(smtc) = self.system_media_transport_controls() {
            let status = match state {
                MediaPlaybackState::Playing => MediaPlaybackStatus::Playing,
                MediaPlaybackState::Paused => MediaPlaybackStatus::Paused,
                MediaPlaybackState::Opening | MediaPlaybackState::Buffering => {
                    MediaPlaybackStatus::Changing
                }
                _ => MediaPlaybackStatus::Stopped,
            };
            let _ = smtc.SetPlaybackStatus(status);
        }
        if matches!(
            state,
            MediaPlaybackState::Playing | MediaPlaybackState::Paused
        ) {
            self.emit(Event::PlaybackStateChanged {
                playing: state == MediaPlaybackState::Playing,
            });
        }
    }

    fn system_media_transport_controls(
        &self,
    ) -> windows_core::Result<SystemMediaTransportControls> {
        self.player
            .cast::<IMediaPlayer2>()?
            .SystemMediaTransportControls()
    }

    /// Rebuilds the runtime track catalog from the active playback item.
    fn populate_track_catalog(&self) {
        let Some(item) = self.item.borrow().clone() else {
            return;
        };
        let mut catalog = TrackCatalog::default();

        if let Ok(tracks) = item.AudioTracks() {
            let infos = (0..tracks.Size().unwrap_or(0))
                .filter_map(|index| tracks.GetAt(index).ok())
                .enumerate()
                .map(|(index, track)| {
                    AudioTrackInfo::new(
                        track_label(
                            &track,
                            "Audio",
                            u32::try_from(index).expect("track index fits in u32"),
                        ),
                        track.language(),
                        Vec::new(),
                    )
                })
                .collect();
            catalog = catalog.replacing_audio(infos);
        }

        if let Ok(tracks) = item.VideoTracks() {
            let infos = (0..tracks.Size().unwrap_or(0))
                .filter_map(|index| tracks.GetAt(index).ok())
                .enumerate()
                .map(|(index, track)| {
                    let id = track.Id().unwrap_or_else(|_| format!("video-{index}"));
                    VideoTrackInfo::new(
                        id,
                        track_label(
                            &track,
                            "Video",
                            u32::try_from(index).expect("track index fits in u32"),
                        ),
                        None,
                        None,
                        Vec::new(),
                        false,
                    )
                })
                .collect();
            catalog = catalog.replacing_video(infos);
        }

        if let Ok(tracks) = item.TimedMetadataTracks() {
            let origin = if self.live.get() {
                SubtitleTrackOrigin::Manifest
            } else {
                SubtitleTrackOrigin::Embedded
            };
            let infos = (0..tracks.Size().unwrap_or(0))
                .filter_map(|index| tracks.GetAt(index).ok())
                .filter(|track| {
                    matches!(
                        track.TimedMetadataKind(),
                        Ok(TimedMetadataKind::Caption | TimedMetadataKind::Subtitle)
                    )
                })
                .enumerate()
                .map(|(index, track)| {
                    SubtitleTrackInfo::new(
                        track_label(
                            &track,
                            "Subtitle",
                            u32::try_from(index).expect("track index fits in u32"),
                        ),
                        track.language(),
                        Vec::new(),
                        false,
                        origin,
                    )
                })
                .collect();
            catalog = catalog.replacing_subtitles(infos);
        }

        self.track_catalog.set(catalog);
    }

    /// Re-reads the seekable ranges into `live_window` for live sources.
    fn update_live_window(&self) {
        let window = self.live.get().then(|| {
            let ranges = self
                .session
                .cast::<IMediaPlaybackSession2>()
                .and_then(|session| session.GetSeekableRanges())
                .ok()?;
            let (start, end) = (0..ranges.Size().unwrap_or(0))
                .filter_map(|index| ranges.GetAt(index).ok())
                .map(|range| (timespan_seconds(range.start), timespan_seconds(range.end)))
                .reduce(|(s0, e0), (s1, e1)| (s0.min(s1), e0.max(e1)))?;
            let position = timespan_seconds(self.session.Position().unwrap_or_default());
            Some(LiveWindow::new(
                Duration::from_secs_f64(start.max(0.0)),
                Duration::from_secs_f64(end.max(0.0)),
                Duration::from_secs_f64(end.max(0.0)),
                Duration::from_secs_f64(position.clamp(start, end).max(0.0)),
            ))
        });
        self.live_window.set(window.flatten());
    }

    /// Applies the user's track selections to the current playback item.
    fn apply_track_selections(&self) {
        let Some(item) = self.item.borrow().clone() else {
            return;
        };
        let selected = |index| i32::try_from(index).expect("track index fits in i32");
        if let Ok(list) = item.AudioTracks()
            && let Ok(list) = list.cast::<ISingleSelectMediaTrackList>()
        {
            match self.audio_track_selection.snapshot() {
                AudioTrackSelection::Auto => {}
                AudioTrackSelection::Track(index) => {
                    let _ = list.SetSelectedIndex(selected(index));
                }
            }
        }
        if let Ok(list) = item.VideoTracks()
            && let Ok(list) = list.cast::<ISingleSelectMediaTrackList>()
        {
            match self.video_track_selection.snapshot() {
                // `-1` hands rendition selection back to the adaptive engine.
                VideoTrackSelection::Auto => {
                    let _ = list.SetSelectedIndex(-1);
                }
                VideoTrackSelection::Track(index) => {
                    let _ = list.SetSelectedIndex(selected(index));
                }
            }
        }
        if let Ok(tracks) = item.TimedMetadataTracks()
            && let Ok(list) = tracks.cast::<IMediaPlaybackTimedMetadataTrackList>()
        {
            let count = tracks.Size().unwrap_or(0);
            match self.subtitle_selection.snapshot() {
                SubtitleSelection::Auto => {}
                SubtitleSelection::Off => {
                    for index in 0..count {
                        let _ = list.SetPresentationMode(
                            index,
                            TimedMetadataTrackPresentationMode::Disabled,
                        );
                    }
                }
                SubtitleSelection::Track(selected) => {
                    for index in 0..count {
                        let mode = if index as usize == selected {
                            TimedMetadataTrackPresentationMode::PlatformPresented
                        } else {
                            TimedMetadataTrackPresentationMode::Disabled
                        };
                        let _ = list.SetPresentationMode(index, mode);
                    }
                }
            }
        }
    }

    /// Maps `MediaMetadata` onto the item's display properties, which the
    /// system transport controls then surface as now-playing info.
    fn apply_display_properties(item: &MediaItem, playback_item: &MediaPlaybackItem) {
        let Ok(props) = playback_item
            .cast::<IMediaPlaybackItem2>()
            .and_then(|item| item.GetDisplayProperties())
        else {
            return;
        };
        let _ = props.SetType(MediaPlaybackType::Video);
        if let Ok(video) = props.VideoProperties()
            && let Some(title) = item.metadata.title()
        {
            let _ = video.SetTitle(title);
        }
        if let Ok(music) = props.MusicProperties() {
            if let Some(artist) = item.metadata.artist() {
                let _ = music.SetArtist(artist);
            }
            if let Ok(music2) = music.cast::<IMusicDisplayProperties2>()
                && let Some(album) = item.metadata.album()
            {
                let _ = music2.SetAlbumTitle(album);
            }
        }
        let _ = playback_item
            .cast::<IMediaPlaybackItem2>()
            .and_then(|item| item.ApplyDisplayProperties(&props));
    }

    /// Pushes `has_next` / `has_previous` into the system transport controls
    /// and the command manager so OS-level next/previous commands engage.
    fn update_navigation(&self) {
        let (next, previous) = (self.has_next.snapshot(), self.has_previous.snapshot());
        if let Ok(smtc) = self.system_media_transport_controls() {
            let _ = smtc.SetIsNextEnabled(next);
            let _ = smtc.SetIsPreviousEnabled(previous);
        }
        if let Ok(manager) = self.player3.CommandManager() {
            if let Ok(behavior) = manager.NextBehavior() {
                let _ = behavior.SetEnablingRule(if next {
                    MediaCommandEnablingRule::Always
                } else {
                    MediaCommandEnablingRule::Never
                });
            }
            if let Ok(behavior) = manager.PreviousBehavior() {
                let _ = behavior.SetEnablingRule(if previous {
                    MediaCommandEnablingRule::Always
                } else {
                    MediaCommandEnablingRule::Never
                });
            }
        }
    }

    fn update_repeat(&self) {
        let looping = self.loops || self.repeat.snapshot() == RepeatMode::One;
        let _ = self.player.SetIsLoopingEnabled(looping);
        if let Ok(smtc) = self
            .system_media_transport_controls()
            .and_then(|s| s.cast::<ISystemMediaTransportControls2>())
        {
            let mode = match self.repeat.snapshot() {
                RepeatMode::Off => MediaPlaybackAutoRepeatMode::None,
                RepeatMode::One => MediaPlaybackAutoRepeatMode::Track,
                RepeatMode::All => MediaPlaybackAutoRepeatMode::List,
            };
            let _ = smtc.SetAutoRepeatMode(mode);
        }
    }

    /// Re-arms `CueEntered` on data/custom timed metadata tracks so
    /// scheme-defined metadata surfaces as [`Event::TimedMetadata`].
    fn subscribe_timed_cues(this: &Rc<Self>, item: &MediaPlaybackItem) {
        this.cue_revokers.borrow_mut().clear();
        let Ok(tracks) = item.TimedMetadataTracks() else {
            return;
        };
        for index in 0..tracks.Size().unwrap_or(0) {
            let Ok(track) = tracks.GetAt(index) else {
                continue;
            };
            if !matches!(
                track.TimedMetadataKind(),
                Ok(TimedMetadataKind::Data | TimedMetadataKind::Custom)
            ) {
                continue;
            }
            let weak = Rc::downgrade(this);
            let revoker = track
                .CueEntered(move |sender, args| {
                    let (Ok(track), Ok(args)) = (sender.ok(), args.ok()) else {
                        return;
                    };
                    let (Ok(cue), Some(this)) = (args.Cue(), weak.upgrade()) else {
                        return;
                    };
                    let message_data = cue
                        .cast::<IDataCue>()
                        .and_then(|data| data.Data())
                        .and_then(|buffer| {
                            let reader = DataReader::FromBuffer(&buffer)?;
                            let mut bytes = vec![0u8; buffer.Length()? as usize];
                            reader.ReadBytes(&mut bytes)?;
                            Ok(bytes)
                        })
                        .unwrap_or_default();
                    let id = cue.Id().ok().and_then(|s| s.parse().ok()).unwrap_or(0);
                    this.emit(Event::TimedMetadata {
                        metadata: TimedMetadata::new(
                            track.DispatchType().unwrap_or_default(),
                            cue.Id().unwrap_or_default(),
                            id,
                            Duration::from_secs_f64(timespan_seconds(
                                cue.StartTime().unwrap_or_default(),
                            )),
                            Duration::from_secs_f64(timespan_seconds(
                                cue.Duration().unwrap_or_default(),
                            )),
                            message_data,
                        ),
                    });
                })
                .expect("TimedMetadataTrack::CueEntered");
            this.cue_revokers.borrow_mut().push(revoker);
        }
    }

    /// Re-applied whenever the track lists change after item creation.
    fn on_tracks_changed(&self) {
        self.populate_track_catalog();
        self.apply_track_selections();
    }

    /// Builds a `MediaPlaybackItem` from a resolved `MediaSource`, wires
    /// per-item events, and hands it to the player.
    fn attach(this: &Rc<Self>, item: &MediaItem, source: &MediaSource, live: bool) {
        this.live.set(live);

        for track in &item.subtitle_tracks {
            if let Ok(uri) = Uri::CreateUri(track.source.as_str())
                && let Ok(timed) = TimedTextSource::CreateFromUri(&uri)
                && let Ok(sources) = source
                    .cast::<IMediaSource2>()
                    .and_then(|s| s.ExternalTimedTextSources())
            {
                let _ = sources.Append(&timed);
            }
        }

        let playback_item = match MediaPlaybackItem::Create(source) {
            Ok(item) => item,
            Err(error) => {
                this.phase.set(PlaybackPhase::Failed);
                this.emit(Event::Error {
                    message: format!("MediaPlaybackItem creation failed: {error}"),
                });
                return;
            }
        };

        Self::apply_display_properties(item, &playback_item);

        // Per-item track events rebuild the catalog and re-apply selections.
        this.item_revokers.borrow_mut().clear();
        let mut revokers = Vec::new();
        {
            let this = Rc::downgrade(this);
            revokers.push(
                playback_item
                    .AudioTracksChanged(move |_, _| {
                        if let Some(this) = this.upgrade() {
                            this.on_tracks_changed();
                        }
                    })
                    .expect("MediaPlaybackItem::AudioTracksChanged"),
            );
        }
        {
            let this = Rc::downgrade(this);
            revokers.push(
                playback_item
                    .VideoTracksChanged(move |_, _| {
                        if let Some(this) = this.upgrade() {
                            this.on_tracks_changed();
                        }
                    })
                    .expect("MediaPlaybackItem::VideoTracksChanged"),
            );
        }
        {
            let this = Rc::downgrade(this);
            revokers.push(
                playback_item
                    .TimedMetadataTracksChanged(move |_, _| {
                        if let Some(this) = this.upgrade() {
                            this.on_tracks_changed();
                            if let Some(item) = this.item.borrow().clone() {
                                Self::subscribe_timed_cues(&this, &item);
                            }
                        }
                    })
                    .expect("MediaPlaybackItem::TimedMetadataTracksChanged"),
            );
        }
        *this.item_revokers.borrow_mut() = revokers;

        *this.item.borrow_mut() = Some(playback_item.clone());
        Self::subscribe_timed_cues(this, &playback_item);
        let _ = this
            .player
            .cast::<IMediaPlayerSource2>()
            .and_then(|player| {
                player.SetSource(
                    &playback_item
                        .cast::<IMediaPlaybackSource>()
                        .expect("MediaPlaybackItem is IMediaPlaybackSource"),
                )
            });
        if this.desired_playing.snapshot() {
            let _ = this.player.Play();
        }
    }

    /// Resolves a `MediaItem` into a `MediaSource` and attaches it.
    ///
    /// Adaptive delivery and local files resolve asynchronously; the
    /// generation counter drops a resolution that loses a race to a newer
    /// `source` value.
    fn load(this: &Rc<Self>, item: MediaItem) {
        let generation = this.source_generation.get().wrapping_add(1);
        this.source_generation.set(generation);
        this.phase.set(PlaybackPhase::Preparing);
        this.live_window.set(None);
        this.track_catalog.set(TrackCatalog::default());
        *this.item.borrow_mut() = None;
        this.item_revokers.borrow_mut().clear();
        this.cue_revokers.borrow_mut().clear();

        let weak = Rc::downgrade(this);
        let initial_bitrate = this.initial_bitrate;
        let queue = this.executor.queue().clone();
        this.executor.spawn_local(async move {
            let resolved = resolve_media_source(&item).await;
            // `WinRT` async completions resume on an arbitrary thread — the
            // player pipeline is only driven on the dispatcher.
            enqueue_on_ui_thread(&queue, move || {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                if this.source_generation.get() != generation {
                    return;
                }
                match resolved
                    .and_then(|resolved| materialize_media_source(&item, resolved, initial_bitrate))
                {
                    Ok((source, live)) => Self::attach(&this, &item, &source, live),
                    Err(message) => {
                        this.phase.set(PlaybackPhase::Failed);
                        this.emit(Event::Error { message });
                    }
                }
            });
        });
    }
}

/// The async half of source resolution — only `WinRT` async calls happen off
/// the dispatcher; `MediaSource` construction happens back on the UI thread.
enum ResolvedSource {
    /// Progressive file via `StorageFile`.
    File(StorageFile),
    /// Progressive stream via `Uri`.
    Uri(Uri),
    /// HLS/DASH via the platform adaptive engine.
    Adaptive(AdaptiveMediaSource),
}

async fn resolve_media_source(item: &MediaItem) -> Result<ResolvedSource, String> {
    let text = item.source.as_str();

    let file_path = if item.source.is_local() {
        Some(text)
    } else if item.source.scheme() == Some("file") {
        // `file:///C:/x` reports its path as `/C:/x` — drop the leading slash
        // when it prefixes a drive letter.
        let path = item.source.path();
        Some(match path.strip_prefix('/') {
            Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest,
            _ => path,
        })
    } else {
        None
    };
    if item.delivery == Delivery::Progressive
        && let Some(path) = file_path
    {
        let file = StorageFile::GetFileFromPathAsync(path)
            .map_err(|error| format!("invalid media path {text}: {error}"))?
            .await
            .map_err(|error| format!("cannot open media file {text}: {error}"))?;
        return Ok(ResolvedSource::File(file));
    }

    let uri = Uri::CreateUri(text).map_err(|error| format!("invalid media URL {text}: {error}"))?;
    if item.delivery == Delivery::Progressive {
        return Ok(ResolvedSource::Uri(uri));
    }

    let result = AdaptiveMediaSource::CreateFromUriAsync(&uri)
        .map_err(|error| format!("adaptive source request failed for {text}: {error}"))?
        .await
        .map_err(|error| format!("adaptive source failed for {text}: {error}"))?;
    if result.Status() != Ok(AdaptiveMediaSourceCreationStatus::Success) {
        return Err(format!(
            "adaptive source creation failed for {text}: {:?}",
            result.Status()
        ));
    }
    result
        .MediaSource()
        .map(ResolvedSource::Adaptive)
        .map_err(|error| format!("adaptive source unavailable for {text}: {error}"))
}

/// Builds the `MediaSource` on the UI thread once resolution completes.
fn materialize_media_source(
    item: &MediaItem,
    resolved: ResolvedSource,
    initial_bitrate: u32,
) -> Result<(MediaSource, bool), String> {
    let text = item.source.as_str();
    match resolved {
        ResolvedSource::File(file) => MediaSource::CreateFromStorageFile(&file)
            .map(|source| (source, false))
            .map_err(|error| format!("media file {text} is not a playable source: {error}")),
        ResolvedSource::Uri(uri) => MediaSource::CreateFromUri(&uri)
            .map(|source| (source, false))
            .map_err(|error| format!("{text} is not a playable source: {error}")),
        ResolvedSource::Adaptive(adaptive) => {
            let _ = adaptive.SetInitialBitrate(initial_bitrate);
            let live = adaptive.IsLive().unwrap_or(false);
            MediaSource::CreateFromAdaptiveMediaSource(&adaptive)
                .map(|source| (source, live))
                .map_err(|error| format!("cannot wrap adaptive source for {text}: {error}"))
        }
    }
}

/// Subscribes `signal`; every change is marshaled onto the UI dispatcher and
/// applied to the coordinator. Returns the initial value and the guard.
fn watch<S, A>(signal: &S, this: &Rc<VideoCoordinator>, apply: A) -> (S::Output, BoxWatcherGuard)
where
    S: Signal<Guard = BoxWatcherGuard>,
    A: Fn(&Rc<VideoCoordinator>, S::Output) + 'static,
{
    let queue = this.executor.queue().clone();
    let weak = Rc::downgrade(this);
    let apply = Rc::new(apply);
    subscribe_then_get(signal, move |ctx| {
        let value = ctx.into_value();
        let weak = weak.clone();
        let apply = apply.clone();
        let queue = queue.clone();
        enqueue_on_ui_thread(&queue, move || {
            if let Some(this) = weak.upgrade() {
                apply(&this, value);
            }
        });
    })
}

/// Builds a `MediaPlayerElement` + `MediaPlayer` pair wired to `playback`.
fn video_element(
    playback: PlaybackConfiguration<Option<BoundVideoEventHandler>>,
    content_mode: ContentMode,
    projection: &VideoProjection,
    show_controls: bool,
    loops: bool,
    renderer: &mut WinUiRenderer,
) -> UIElement {
    assert!(
        !projection.is_spherical(),
        "spherical video projection has no MediaPlayerElement realization on WinUI"
    );

    let element = MediaPlayerElement::new().expect("MediaPlayerElement::new");
    element.SetAutoPlay(false).expect("SetAutoPlay");
    element
        .SetAreTransportControlsEnabled(show_controls)
        .expect("SetAreTransportControlsEnabled");
    element
        .SetStretch(content_mode_stretch(content_mode))
        .expect("MediaPlayerElement::SetStretch");
    let ui_element: UIElement = element.cast().expect("MediaPlayerElement is a UIElement");
    let fe = framework(&ui_element);

    let player = MediaPlayer::new().expect("MediaPlayer::new");
    let player3 = player.cast::<IMediaPlayer3>().expect("IMediaPlayer3");
    player3
        .SetRealTimePlayback(playback.playback_policy.realtime)
        .expect("MediaPlayer::SetRealTimePlayback");
    let session = player3
        .PlaybackSession()
        .expect("MediaPlayer::PlaybackSession");
    element
        .SetMediaPlayer(&player)
        .expect("MediaPlayerElement::SetMediaPlayer");

    if playback.playback_policy.power != PlaybackPowerPolicy::PlatformManaged {
        if let Some(handler) = &playback.on_event {
            handler.call(Event::Error {
                message: String::from(
                    "the playback power policy requires a hardware offload or tunnel \
                     that Windows.Media.Playback cannot guarantee",
                ),
            });
        }
        playback.phase.set(PlaybackPhase::Failed);
    }

    let initial_bitrate = u32::try_from(playback.playback_policy.network.initial_bandwidth().get())
        .unwrap_or(u32::MAX);

    let coordinator = Rc::new(VideoCoordinator {
        player,
        player3,
        session,
        controller: playback.controller.clone(),
        on_event: playback.on_event,
        executor: renderer.executor().clone(),
        initial_bitrate,
        item: RefCell::new(None),
        item_revokers: RefCell::new(Vec::new()),
        cue_revokers: RefCell::new(Vec::new()),
        loops,
        live: Cell::new(false),
        source_generation: Cell::new(0),
        source: playback.source,
        desired_playing: playback.desired_playing,
        seek_target_seconds: playback.seek_target_seconds,
        seek_generation: playback.seek_generation,
        step_forward_generation: playback.step_forward_generation,
        step_backward_generation: playback.step_backward_generation,
        volume: playback.volume,
        muted: playback.muted,
        playback_rate: playback.playback_rate,
        repeat: playback.repeat,
        shuffle: playback.shuffle,
        has_next: playback.has_next,
        has_previous: playback.has_previous,
        audio_track_selection: playback.audio_track_selection,
        video_track_selection: playback.video_track_selection,
        subtitle_selection: playback.subtitle_selection,
        position_seconds: playback.position_seconds,
        duration_seconds: playback.duration_seconds,
        phase: playback.phase,
        track_catalog: playback.track_catalog,
        live_window: playback.live_window,
    });

    VideoCoordinator::wire_events(&coordinator, &fe);
    let guards = VideoCoordinator::wire_bindings(&coordinator);

    store_retained(&fe, Box::new(coordinator));
    store_watcher_guards(&fe, guards);
    ui_element
}

impl VideoCoordinator {
    /// Lifts a coordinator action into a `WinRT` event handler that only runs
    /// while the coordinator is alive.
    fn on<T, A>(
        this: &Rc<Self>,
        f: impl Fn(&Self, &A) + 'static,
    ) -> impl Fn(windows_core::Ref<'_, T>, windows_core::Ref<'_, A>) + 'static
    where
        T: Interface + 'static,
        A: Interface + 'static,
    {
        let weak = Rc::downgrade(this);
        move |_, args| {
            let (Some(this), Ok(args)) = (weak.upgrade(), args.ok()) else {
                return;
            };
            f(&this, args);
        }
    }

    /// Wires `MediaPlayer`, `MediaPlaybackSession`, and command-manager events
    /// onto the shared bindings and `on_event`.
    fn wire_events(this: &Rc<Self>, fe: &FrameworkElement) {
        Self::wire_player_events(this, fe);
        Self::wire_session_events(this, fe);
        Self::wire_commands(this, fe);
    }

    fn wire_player_events(this: &Rc<Self>, fe: &FrameworkElement) {
        store_event_revoker(
            fe,
            this.player
                .MediaOpened(Self::on::<MediaPlayer, windows_core::IInspectable>(
                    this,
                    |this, _| {
                        this.populate_track_catalog();
                        this.apply_track_selections();
                        if let Ok(duration) = this.session.NaturalDuration() {
                            this.duration_seconds.set(timespan_seconds(duration));
                        }
                        this.update_live_window();
                        this.phase.set(PlaybackPhase::Ready);
                        this.emit(Event::PlaybackOutputPathChanged {
                            path: PlaybackOutputPath::PlatformManaged,
                        });
                        this.emit(Event::ReadyToPlay);
                    },
                ))
                .expect("MediaPlayer::MediaOpened"),
        );
        store_event_revoker(
            fe,
            this.player
                .MediaEnded(Self::on::<MediaPlayer, windows_core::IInspectable>(
                    this,
                    |this, _| {
                        this.phase.set(PlaybackPhase::Ended);
                        this.emit(Event::Ended);
                        // `One` loops inside the pipeline; `All` advances through
                        // the controller, which wraps at the end.
                        if this.repeat.snapshot() != RepeatMode::One && this.has_next.snapshot() {
                            let _ = this.controller.next();
                        }
                    },
                ))
                .expect("MediaPlayer::MediaEnded"),
        );
        store_event_revoker(
            fe,
            this.player
                .MediaFailed(Self::on::<MediaPlayer, MediaPlayerFailedEventArgs>(
                    this,
                    |this, args| {
                        this.phase.set(PlaybackPhase::Failed);
                        this.emit(Event::Error {
                            message: args
                                .ErrorMessage()
                                .unwrap_or_else(|_| String::from("media playback failed")),
                        });
                    },
                ))
                .expect("MediaPlayer::MediaFailed"),
        );
        store_event_revoker(
            fe,
            this.player
                .BufferingStarted(Self::on::<MediaPlayer, windows_core::IInspectable>(
                    this,
                    |this, _| {
                        this.phase.set(PlaybackPhase::Buffering);
                        this.emit(Event::Buffering);
                    },
                ))
                .expect("MediaPlayer::BufferingStarted"),
        );
        store_event_revoker(
            fe,
            this.player
                .BufferingEnded(Self::on::<MediaPlayer, windows_core::IInspectable>(
                    this,
                    |this, _| {
                        this.emit(Event::BufferingEnded);
                    },
                ))
                .expect("MediaPlayer::BufferingEnded"),
        );
    }

    fn wire_session_events(this: &Rc<Self>, fe: &FrameworkElement) {
        store_event_revoker(
            fe,
            this.session
                .PlaybackStateChanged(
                    Self::on::<MediaPlaybackSession, windows_core::IInspectable>(
                        this,
                        |this, _| {
                            if let Ok(state) = this.session.PlaybackState() {
                                this.on_session_state(state);
                            }
                        },
                    ),
                )
                .expect("MediaPlaybackSession::PlaybackStateChanged"),
        );
        store_event_revoker(
            fe,
            this.session
                .PositionChanged(
                    Self::on::<MediaPlaybackSession, windows_core::IInspectable>(
                        this,
                        |this, _| {
                            if let Ok(position) = this.session.Position() {
                                this.position_seconds.set(timespan_seconds(position));
                            }
                        },
                    ),
                )
                .expect("MediaPlaybackSession::PositionChanged"),
        );
        store_event_revoker(
            fe,
            this.session
                .NaturalDurationChanged(
                    Self::on::<MediaPlaybackSession, windows_core::IInspectable>(
                        this,
                        |this, _| {
                            if let Ok(duration) = this.session.NaturalDuration() {
                                this.duration_seconds.set(timespan_seconds(duration));
                            }
                        },
                    ),
                )
                .expect("MediaPlaybackSession::NaturalDurationChanged"),
        );
        store_event_revoker(
            fe,
            this.session
                .BufferingProgressChanged(Self::on::<
                    MediaPlaybackSession,
                    windows_core::IInspectable,
                >(this, |this, _| {
                    if let (Ok(progress), Ok(duration), Ok(position)) = (
                        this.session.BufferingProgress(),
                        this.session.NaturalDuration(),
                        this.session.Position(),
                    ) {
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let buffered_ms = ((progress * timespan_seconds(duration)
                            - timespan_seconds(position))
                            * 1000.0)
                            .max(0.0) as u32;
                        this.emit(Event::BufferLevel { buffered_ms });
                    }
                }))
                .expect("MediaPlaybackSession::BufferingProgressChanged"),
        );
        if let Ok(session2) = this.session.cast::<IMediaPlaybackSession2>() {
            store_event_revoker(
                fe,
                session2
                    .SeekableRangesChanged(Self::on::<
                        MediaPlaybackSession,
                        windows_core::IInspectable,
                    >(this, |this, _| {
                        this.update_live_window();
                    }))
                    .expect("MediaPlaybackSession::SeekableRangesChanged"),
            );
        }
    }

    /// `MediaPlaybackCommandManager` routes OS-level and transport-control
    /// next/previous commands through the `WaterUI` controller.
    fn wire_commands(this: &Rc<Self>, fe: &FrameworkElement) {
        let Ok(manager) = this.player3.CommandManager() else {
            return;
        };
        store_event_revoker(
            fe,
            manager
                .NextReceived(Self::on::<
                    MediaPlaybackCommandManager,
                    MediaPlaybackCommandManagerNextReceivedEventArgs,
                >(this, |this, args| {
                    let _ = args.SetHandled(true);
                    this.emit(Event::NextRequested);
                    let _ = this.controller.next();
                }))
                .expect("MediaPlaybackCommandManager::NextReceived"),
        );
        store_event_revoker(
            fe,
            manager
                .PreviousReceived(Self::on::<
                    MediaPlaybackCommandManager,
                    MediaPlaybackCommandManagerPreviousReceivedEventArgs,
                >(this, |this, args| {
                    let _ = args.SetHandled(true);
                    this.emit(Event::PreviousRequested);
                    let _ = this.controller.previous();
                }))
                .expect("MediaPlaybackCommandManager::PreviousReceived"),
        );
    }

    /// Watches every command binding; returned guards are stored on the
    /// element so subscriptions live exactly as long as the view.
    fn wire_bindings(this: &Rc<Self>) -> Vec<BoxWatcherGuard> {
        let mut guards = Self::wire_transport(this);
        guards.extend(Self::wire_selections(this));
        guards
    }

    /// Transport command bindings: source, play/pause, seek, steps, volume,
    /// mute, rate, repeat, shuffle, and queue navigation.
    fn wire_transport(this: &Rc<Self>) -> Vec<BoxWatcherGuard> {
        let mut guards = Vec::new();

        let (initial, guard) = watch(&this.source, this, Self::load);
        guards.push(guard);
        Self::load(this, initial);

        let (initial, guard) = watch(&this.desired_playing, this, |this, playing| {
            let _ = if playing {
                this.player.Play()
            } else {
                this.player.Pause()
            };
        });
        guards.push(guard);
        if initial {
            let _ = this.player.Play();
        }

        guards.push(
            watch(&this.seek_generation, this, |this, _| {
                let _ = this
                    .session
                    .SetPosition(seconds_timespan(this.seek_target_seconds.snapshot()));
            })
            .1,
        );
        guards.push(
            watch(&this.step_forward_generation, this, |this, _| {
                let _ = this.player3.StepForwardOneFrame();
            })
            .1,
        );
        guards.push(
            watch(&this.step_backward_generation, this, |this, _| {
                let _ = this.player3.StepBackwardOneFrame();
            })
            .1,
        );
        let (initial, guard) = watch(&this.volume, this, |this, volume| {
            let _ = this.player.SetVolume(f64::from(volume.level()));
        });
        guards.push(guard);
        let _ = this.player.SetVolume(f64::from(initial.level()));
        let (initial, guard) = watch(&this.muted, this, |this, muted| {
            let _ = this.player.SetIsMuted(muted);
        });
        guards.push(guard);
        let _ = this.player.SetIsMuted(initial);
        let (initial, guard) = watch(&this.playback_rate, this, |this, rate| {
            let _ = this.session.SetPlaybackRate(f64::from(rate));
        });
        guards.push(guard);
        let _ = this.session.SetPlaybackRate(f64::from(initial));

        guards.push(
            watch(&this.repeat, this, |this, _| {
                this.update_repeat();
            })
            .1,
        );
        this.update_repeat();

        let (initial, guard) = watch(&this.shuffle, this, |this, shuffle| {
            if let Ok(smtc) = this
                .system_media_transport_controls()
                .and_then(|s| s.cast::<ISystemMediaTransportControls2>())
            {
                let _ = smtc.SetShuffleEnabled(shuffle);
            }
        });
        guards.push(guard);
        if let Ok(smtc) = this
            .system_media_transport_controls()
            .and_then(|s| s.cast::<ISystemMediaTransportControls2>())
        {
            let _ = smtc.SetShuffleEnabled(initial);
        }
        let _ = this
            .system_media_transport_controls()
            .and_then(|s| s.SetIsEnabled(true));

        for binding in [&this.has_next, &this.has_previous] {
            guards.push(
                watch(binding, this, |this, _| {
                    this.update_navigation();
                })
                .1,
            );
        }
        this.update_navigation();
        guards
    }

    /// Track-selection bindings: audio, video, and subtitle.
    fn wire_selections(this: &Rc<Self>) -> Vec<BoxWatcherGuard> {
        [
            watch(&this.audio_track_selection, this, |this, _| {
                this.apply_track_selections();
            })
            .1,
            watch(&this.video_track_selection, this, |this, _| {
                this.apply_track_selections();
            })
            .1,
            watch(&this.subtitle_selection, this, |this, _| {
                this.apply_track_selections();
            })
            .1,
        ]
        .into()
    }
}

impl WinUiComponent for Native<NativeVideoConfig> {
    /// Renders a raw `MediaPlayerElement` without transport controls.
    fn render(self, _env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let config = self.into_inner();
        video_element(
            config.playback,
            config.content_mode,
            &config.projection,
            false,
            config.loops,
            renderer,
        )
    }
}

impl WinUiComponent for Native<NativeVideoPlayerConfig> {
    /// Renders a `MediaPlayerElement` with `MediaTransportControls`.
    fn render(self, _env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let config = self.into_inner();
        video_element(
            config.playback,
            config.content_mode,
            &config.projection,
            config.show_controls,
            false,
            renderer,
        )
    }
}
