//! Dropping a dragged song on a playlist, wherever that playlist is drawn.

use gpui::prelude::*;
use gpui::{App, Window};
use music::Shape;
use state::{Leaving, Shelf, Sonora};
use ui::{ActiveTheme as _, DraggedPin, Pin, PinKind};

use crate::shared::pins::leaving;
use crate::shared::playlist_editor::{Edit, PlaylistEditor};

/// How much of the accent a page takes on while a song held over it would join its playlist.
const PAGE_TINT: f32 = 0.08;

/// Whether the playlist takes the dragged `song`: one the listener may edit, on the same shelf
/// as the song, since a provider adds only its own tracks.
pub(crate) fn accepts(song: &Pin, playlist_id: &str, cx: &App) -> bool {
    song.kind == PinKind::Song
        && Shelf::of(&song.id) == Shelf::of(playlist_id)
        && Sonora::global(cx).library.read(cx).is_editable(playlist_id)
}

/// Like `accepts`, but leaves out a song dragged from the playlist itself, whose own page takes
/// such a drag as a new place in the order.
pub(crate) fn welcomes(song: &Pin, playlist_id: &str, cx: &App) -> bool {
    song.origin.as_deref() != Some(playlist_id) && accepts(song, playlist_id, cx)
}

/// Adds a dropped song to a playlist, and asks first when the playlist holds the song already.
/// Shift held on the drop moves the song instead.
pub(crate) fn add(playlist_id: &str, song: &Pin, window: &mut Window, cx: &mut App) {
    let library = Sonora::global(cx).library.clone();
    let Some(playlist) = library.read(cx).playlist(playlist_id).cloned() else {
        return;
    };
    let track = song.id.clone();
    let leaving = window
        .modifiers()
        .shift
        .then(|| leaves(song, &playlist.id, cx))
        .flatten();
    match library.read(cx).holds(&playlist.id, &track) == Some(true) {
        true => PlaylistEditor::open(
            Edit::Again {
                playlist,
                track,
                leaving,
            },
            window,
            cx,
        ),
        false => library.update(cx, |library, cx| match leaving {
            Some(leaving) => library.move_to_playlist(playlist.id, track, leaving, cx),
            None => library.add_to_playlist(playlist.id, track, cx),
        }),
    }
}

/// An element that stands for a playlist and adds a song dropped on it.
pub(crate) trait TakesSongs: InteractiveElement + Sized {
    /// Adds a song dropped on this card to the playlist, lighting the card while one is held
    /// over it.
    fn takes_songs(self, playlist_id: &str) -> Self {
        let (held, dropped) = (playlist_id.to_owned(), playlist_id.to_owned());
        self.drag_over::<DraggedPin>(move |style, dragged, _, cx| {
            match accepts(&dragged.pin, &held, cx) {
                true => style.bg(cx.theme().table_active),
                false => style,
            }
        })
        .on_drop(move |dragged: &DraggedPin, window, cx| {
            if accepts(&dragged.pin, &dropped, cx) {
                add(&dropped, &dragged.pin, window, cx);
            }
        })
    }

    /// Adds a song dropped anywhere on this page to the playlist the page shows, tinting the
    /// page while one is held over it. A song of the playlist itself is left alone.
    fn gathers_songs(self, playlist_id: &str) -> Self {
        let (held, dropped) = (playlist_id.to_owned(), playlist_id.to_owned());
        self.drag_over::<DraggedPin>(move |style, dragged, _, cx| {
            match welcomes(&dragged.pin, &held, cx) {
                true => style.bg(cx.theme().primary.opacity(PAGE_TINT)),
                false => style,
            }
        })
        .on_drop(move |dragged: &DraggedPin, window, cx| {
            if welcomes(&dragged.pin, &dropped, cx) {
                add(&dropped, &dragged.pin, window, cx);
            }
        })
    }
}

impl<T: InteractiveElement> TakesSongs for T {}

/// What a moved song leaves. The favorites count only on a `Shape::Saved` shelf, since a
/// catalog page lists every song.
fn leaves(song: &Pin, target: &str, cx: &App) -> Option<Leaving> {
    let library = Sonora::global(cx).library.read(cx);
    match leaving(song.origin.as_deref()?) {
        Leaving::Liked => {
            (library.shape(Shelf::Streaming) == Shape::Saved).then_some(Leaving::Liked)
        }
        Leaving::Playlist(id) => {
            (id != target && library.is_editable(&id)).then_some(Leaving::Playlist(id))
        }
    }
}
