use std::cmp::Reverse;

use gpui::{App, Context, Entity};
use music::Playlist;

use crate::devices::Devices;
use crate::library::{Library, LibraryEvent, stamp};
use crate::playback::{Origin, Playback, PlaybackEvent, Whence};
use crate::settings::AppSettings;

/// When each playlist was last used: played, or given or rid of tracks. A list of playlists to
/// choose from puts the ones used last on top.
///
/// Sonora keeps its own stamps, since no provider reports plays and several report no changes
/// either. A change the provider does report, such as one made in another app, counts as well.
pub struct Recency {
    settings: Entity<AppSettings>,
    /// The playlist the account's other device was last seen playing, so it is stamped once
    /// rather than on every report of its progress.
    elsewhere: Option<String>,
}

impl Recency {
    pub fn new(
        settings: Entity<AppSettings>,
        library: Entity<Library>,
        playback: Entity<Playback>,
        devices: Entity<Devices>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.subscribe(&playback, |this, playback, event, cx| {
            let PlaybackEvent::StartedPlayback = event else {
                return;
            };
            if let Some(id) = playlist_of(playback.read(cx).origin(cx)) {
                this.stamp(id, cx);
            }
        })
        .detach();
        cx.observe(&devices, |this, devices, cx| {
            let playing = playlist_of(devices.read(cx).origin(cx).as_ref());
            if this.elsewhere == playing {
                return;
            }
            this.elsewhere = playing.clone();
            if let Some(id) = playing {
                this.stamp(id, cx);
            }
        })
        .detach();
        cx.subscribe(&library, |this, _, event, cx| match event {
            LibraryEvent::PlaylistCreated(id)
            | LibraryEvent::PlaylistReordered(id)
            | LibraryEvent::TrackAdded { playlist: id }
            | LibraryEvent::TrackDropped { playlist: id, .. } => this.stamp(id.clone(), cx),
            _ => {}
        })
        .detach();

        Self {
            settings,
            elsewhere: None,
        }
    }

    /// Puts the playlists used last first. Ones never used keep the order they came in, after
    /// the rest.
    pub fn sort(&self, playlists: &mut [Playlist], cx: &App) {
        let settings = self.settings.read(cx);
        playlists.sort_by_cached_key(|playlist| {
            Reverse(last_used(settings.playlist_use(&playlist.id), playlist))
        });
    }

    fn stamp(&self, id: String, cx: &mut Context<Self>) {
        self.settings
            .update(cx, |settings, cx| settings.use_playlist(id, stamp(), cx));
    }
}

/// The later of Sonora's own stamp and the provider's, in seconds. The provider's counts only for
/// a playlist the listener can change, since a mix or a radio is made afresh every day.
fn last_used(stamped: Option<i64>, playlist: &Playlist) -> Option<i64> {
    match playlist.takes_tracks() {
        true => stamped.max(playlist.modified_at),
        false => stamped,
    }
}

/// The id of the playlist `origin` stands for, when it is one.
fn playlist_of(origin: Option<&Origin>) -> Option<String> {
    origin
        .filter(|origin| origin.whence == Whence::Playlist)
        .map(|origin| origin.id.clone())
}
