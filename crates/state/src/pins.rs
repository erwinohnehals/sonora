use std::cell::OnceCell;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::rc::Rc;

use gpui::{App, Context, Entity};
use music::{PinTarget, PinTargetKind};
use serde::{Deserialize, Serialize};
use ui::{Pin, PinKind};

use crate::library::{Library, LibraryEvent, Shelf};
use crate::session::Session;
use crate::settings::AppSettings;
use crate::{Outcome, Target, Toast, Toasts};

/// An automatic order for the pinned list. No sort at all means the order the user dragged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinSort {
    Alphabetical,
    Kind,
}

impl PinSort {
    pub const ALL: [Self; 2] = [Self::Alphabetical, Self::Kind];

    /// The stored id. Anything else, the empty string included, means the dragged order.
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|sort| sort.id() == id)
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Alphabetical => "alphabetical",
            Self::Kind => "kind",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Alphabetical => "nav-pins-alphabetical",
            Self::Kind => "nav-pins-kind",
        }
    }
}

/// A folder in the pinned list. Folders are Sonora's own, so no provider hears of them, and one
/// can hold pins of any provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinFolder {
    pub id: u64,
    pub name: String,
    /// Whether the sidebar lists the folder's pins under it.
    #[serde(default)]
    pub open: bool,
}

/// One line of the pinned list as the sidebar draws it: the folders first, each followed by its
/// pins while it is open, then the pins in no folder.
#[derive(Clone, Debug)]
pub enum PinRow {
    /// A folder, with how many pins it holds.
    Folder { folder: PinFolder, count: usize },
    /// A pin. `slot` counts it among the pins of its folder, or among the pins in none, and
    /// `last` marks the final one there, below which a drop past the end is drawn.
    Pin {
        pin: Pin,
        folder: Option<u64>,
        slot: usize,
        last: bool,
    },
}

/// The pins grouped the way the sidebar shows them: each folder with its pins, then the pins in
/// no folder.
type Grouped = (Vec<(PinFolder, Vec<Pin>)>, Vec<Pin>);

/// The one pinned list, whatever provider each entry came from. Order is kept locally and covers
/// every entry at once, so a provider that cannot reorder its own pins still follows the drag, and
/// a streamed pin can sit between two local ones.
///
/// A provider that keeps pins of its own is told about every change, and the local list is put
/// back when it refuses.
pub struct Pins {
    settings: Entity<AppSettings>,
    library: Entity<Library>,
    session: Entity<Session>,
    /// What the provider last reported as pinned, by uri, so only a change since then is
    /// followed.
    mirrored: HashMap<String, Pin>,
    /// `rows` as last laid out. Cleared whenever the settings, the session or the library
    /// move, so a frame reads the list without rebuilding it.
    laid: OnceCell<Rc<Vec<PinRow>>>,
    /// `library` as last laid out, cleared alongside `laid`.
    rest: OnceCell<Rc<Vec<Pin>>>,
    /// The digest of what the shelves last listed, so a library notify that changed nothing
    /// the sidebar shows does not rebuild it.
    seen: u64,
    /// Pins that left with a playlist taken out of the library, by playlist id, so an Undo
    /// that brings the playlist back brings its pin back to the same slot.
    parted: HashMap<String, Parted>,
    /// The toast reporting the last unpin, and the pin it reports.
    unpinned: Option<(usize, Pin)>,
}

/// A pin as it was stored, with its provider, its folder and its slot in the dragged order of
/// that folder.
#[derive(Clone)]
struct Parted {
    slug: &'static str,
    pin: Pin,
    folder: Option<u64>,
    gap: usize,
}

impl Pins {
    pub fn new(
        settings: Entity<AppSettings>,
        library: Entity<Library>,
        session: Entity<Session>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.subscribe(&library, |this, _, event, cx| match event {
            LibraryEvent::PlaylistGone(id) => {
                let pin = Pin::new(PinKind::Playlist, id, "");
                if let Some(slug) = this.session.read(cx).slug_for(id) {
                    if let Some(parted) = this.part(slug, &pin, cx) {
                        this.parted.insert(id.clone(), parted);
                    }
                    this.settings
                        .update(cx, |settings, cx| settings.unpin(slug, &pin, cx));
                }
                this.mirrored.retain(|_, held| !held.same(&pin));
                this.changed(cx);
            }
            LibraryEvent::PlaylistBack(id) => {
                if let Some(parted) = this.parted.remove(id) {
                    this.restore(parted, cx);
                }
            }
            _ => {}
        })
        .detach();
        cx.observe(&library, |this, _, cx| {
            this.absorb(cx);
            let seen = this.fingerprint(cx);
            if this.seen != seen {
                this.seen = seen;
                this.changed(cx);
            }
        })
        .detach();
        cx.observe(&settings, |this, _, cx| this.changed(cx))
            .detach();
        cx.observe(&session, |this, _, cx| this.changed(cx))
            .detach();

        Self {
            settings,
            library,
            session,
            mirrored: HashMap::new(),
            laid: OnceCell::new(),
            rest: OnceCell::new(),
            seen: 0,
            parted: HashMap::new(),
            unpinned: None,
        }
    }

    /// The folders and every pin of the live providers, laid out the way the user asked for. A
    /// closed folder's pins are left out. The list is built once per change and shared
    /// afterwards.
    pub fn rows(&self, cx: &App) -> Rc<Vec<PinRow>> {
        self.laid
            .get_or_init(|| {
                let (folders, loose) = self.grouped(cx);
                let mut rows = Vec::new();
                for (folder, pins) in folders {
                    let id = folder.id;
                    let open = folder.open;
                    rows.push(PinRow::Folder {
                        folder,
                        count: pins.len(),
                    });
                    if open {
                        rows.extend(listed(pins, Some(id)));
                    }
                }
                rows.extend(listed(loose, None));
                Rc::new(rows)
            })
            .clone()
    }

    /// The folders in the order the sidebar shows them.
    pub fn folders(&self, cx: &App) -> Vec<PinFolder> {
        self.rows(cx)
            .iter()
            .filter_map(|row| match row {
                PinRow::Folder { folder, .. } => Some(folder.clone()),
                PinRow::Pin { .. } => None,
            })
            .collect()
    }

    /// The folder `pin` is filed in, if any.
    pub fn folder_of(&self, pin: &Pin, cx: &App) -> Option<u64> {
        self.settings.read(cx).folder_of(pin)
    }

    /// Everything the live shelves hold that is not pinned: albums, artists and playlists, laid
    /// out the same way the pins above them are. Nothing else a provider lists belongs here,
    /// since the sidebar can only open these three. Built once per change, like `rows`.
    pub fn library(&self, cx: &App) -> Rc<Vec<Pin>> {
        self.rest
            .get_or_init(|| Rc::new(self.gather_library(cx)))
            .clone()
    }

    /// A digest of the albums, artists and playlists the live shelves list, with the names
    /// and covers the sidebar shows for them.
    fn fingerprint(&self, cx: &App) -> u64 {
        let mut hasher = DefaultHasher::new();
        let library = self.library.read(cx);
        for shelf in [Shelf::Streaming, Shelf::Local] {
            if self.session.read(cx).client_of(shelf).is_none() {
                continue;
            }
            let held = library.state(shelf);
            for playlist in held.playlists() {
                (&playlist.id, &playlist.name, &playlist.cover).hash(&mut hasher);
            }
            for album in held.albums() {
                (&album.id, &album.name, &album.cover_large, &album.cover).hash(&mut hasher);
            }
            for artist in held.artists() {
                (&artist.id, &artist.name, &artist.cover).hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    /// Drops both laid-out lists so the next read rebuilds them.
    fn forget(&mut self) {
        self.laid.take();
        self.rest.take();
    }

    /// Forgets the laid-out lists and tells the observers.
    fn changed(&mut self, cx: &mut Context<Self>) {
        self.forget();
        cx.notify();
    }

    fn gather_library(&self, cx: &App) -> Vec<Pin> {
        let pinned = self.dragged(cx);
        let library = self.library.read(cx);
        let mut rest = Vec::new();
        for shelf in [Shelf::Streaming, Shelf::Local] {
            if self.session.read(cx).client_of(shelf).is_none() {
                continue;
            }
            let held = library.state(shelf);
            rest.extend(held.playlists().iter().map(|playlist| {
                Pin::new(PinKind::Playlist, &playlist.id, &playlist.name)
                    .cover(playlist.cover.clone())
            }));
            rest.extend(held.albums().iter().map(|album| {
                Pin::new(PinKind::Album, &album.id, &album.name)
                    .cover(album.cover_large.clone().or_else(|| album.cover.clone()))
            }));
            rest.extend(held.artists().iter().map(|artist| {
                Pin::new(PinKind::Artist, &artist.id, &artist.name).cover(artist.cover.clone())
            }));
        }
        rest.retain(|pin| !pinned.iter().any(|held| held.same(pin)));
        self.lay_out(&mut rest, cx);
        rest
    }

    /// The folders with their pins, and the pins in none, each laid out the way the user asked
    /// for. A sort orders the folders by name too.
    fn grouped(&self, cx: &App) -> Grouped {
        let slugs = self.session.read(cx).active_slugs();
        let settings = self.settings.read(cx);
        let mut folders: Vec<(PinFolder, Vec<Pin>)> = settings
            .pin_folders()
            .iter()
            .map(|folder| (folder.clone(), Vec::new()))
            .collect();
        let mut loose = Vec::new();
        for (folder, pin) in settings.filed(&slugs) {
            let home = folder.and_then(|id| folders.iter_mut().find(|(held, _)| held.id == id));
            match home {
                Some((_, pins)) => pins.push(pin),
                None => loose.push(pin),
            }
        }
        for (_, pins) in &mut folders {
            self.lay_out(pins, cx);
        }
        self.lay_out(&mut loose, cx);
        if self.sorted(cx) {
            folders.sort_by_key(|(folder, _)| folder.name.to_lowercase());
            if self.reversed(cx) {
                folders.reverse();
            }
        }
        (folders, loose)
    }

    /// The pins in the order they are stored in, whatever the current sort shows.
    fn dragged(&self, cx: &App) -> Vec<Pin> {
        let slugs = self.session.read(cx).active_slugs();
        self.settings.read(cx).pinned(&slugs)
    }

    /// The automatic order in effect, or `None` for the order the user dragged.
    pub fn sort(&self, cx: &App) -> Option<PinSort> {
        self.settings.read(cx).sidebar_pin_sort()
    }

    pub fn reversed(&self, cx: &App) -> bool {
        self.settings.read(cx).sidebar_pin_reversed()
    }

    pub fn sorted(&self, cx: &App) -> bool {
        self.sort(cx).is_some()
    }

    /// Applies the automatic order, if there is one. Without one the list keeps the order it
    /// arrived in, which for the pins is the one the user dragged.
    fn lay_out(&self, pins: &mut [Pin], cx: &App) {
        match self.sort(cx) {
            Some(PinSort::Alphabetical) => pins.sort_by_key(|pin| pin.title.to_lowercase()),
            Some(PinSort::Kind) => {
                pins.sort_by_key(|pin| (rank(pin.kind), pin.title.to_lowercase()))
            }
            None => return,
        }
        if self.reversed(cx) {
            pins.reverse();
        }
    }

    /// Steps one order along: off, then down, then off again. Picking another order starts it
    /// from the top.
    pub fn choose(&mut self, sort: PinSort, cx: &mut Context<Self>) {
        let next = match (self.sort(cx) == Some(sort), self.reversed(cx)) {
            (true, false) => Some((sort, true)),
            (true, true) => None,
            (false, _) => Some((sort, false)),
        };
        let (sort, reversed) = match next {
            Some((sort, reversed)) => (Some(sort), reversed),
            None => (None, false),
        };
        self.settings.update(cx, |settings, cx| {
            settings.set_sidebar_pin_sort(sort, reversed, cx)
        });
        self.changed(cx);
    }

    pub fn holds(&self, pin: &Pin, cx: &App) -> bool {
        self.dragged(cx).iter().any(|held| held.same(pin))
    }

    pub fn toggle(&mut self, pin: Pin, cx: &mut Context<Self>) {
        match self.holds(&pin, cx) {
            true => self.unpin(pin, cx),
            false => self.place(pin, None, None, cx),
        }
    }

    /// Pins into `folder` at `gap`, a slot counted among the pins already shown there, or moves an
    /// existing pin there. No folder means the pins filed in none. `None` appends, and never
    /// moves a pin that is already in the list.
    pub fn place(
        &mut self,
        pin: Pin,
        folder: Option<u64>,
        gap: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let Some(slug) = self.session.read(cx).slug_for(&pin.id) else {
            return;
        };
        let known = self.holds(&pin, cx);
        match gap {
            Some(_) => self.settle(cx),
            None if known => return,
            None => {}
        }
        let slugs = self.session.read(cx).active_slugs();
        self.settings.update(cx, |settings, cx| {
            settings.pin(slug, pin.clone(), folder, gap, &slugs, cx)
        });
        if !known {
            self.tell(&pin, true, folder, cx);
        }
        self.changed(cx);
    }

    /// Moves `pin` to the end of `folder`, or of the pins in no folder, pinning it first when it
    /// is not pinned yet.
    pub fn file(&mut self, pin: Pin, folder: Option<u64>, cx: &mut Context<Self>) {
        let Some(slug) = self.session.read(cx).slug_for(&pin.id) else {
            return;
        };
        let known = self.holds(&pin, cx);
        if known && self.folder_of(&pin, cx) == folder {
            return;
        }
        let slugs = self.session.read(cx).active_slugs();
        self.settings.update(cx, |settings, cx| {
            settings.pin(slug, pin.clone(), folder, None, &slugs, cx)
        });
        if !known {
            self.tell(&pin, true, folder, cx);
        }
        self.changed(cx);
    }

    /// Makes a folder named `name`, and files `pin` in it when one is given.
    pub fn create_folder(&mut self, name: String, pin: Option<Pin>, cx: &mut Context<Self>) {
        let id = self
            .settings
            .update(cx, |settings, cx| settings.add_pin_folder(name, cx));
        if let Some(pin) = pin {
            self.file(pin, Some(id), cx);
        }
        self.changed(cx);
    }

    pub fn rename_folder(&mut self, id: u64, name: String, cx: &mut Context<Self>) {
        self.settings
            .update(cx, |settings, cx| settings.rename_pin_folder(id, name, cx));
        self.changed(cx);
    }

    /// Drops a folder and leaves its pins pinned, in no folder, then reports it in a toast whose
    /// Undo puts the folder back with its pins.
    pub fn remove_folder(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some((folder, slot, held)) = self
            .settings
            .update(cx, |settings, cx| settings.remove_pin_folder(id, cx))
        else {
            return;
        };
        self.changed(cx);

        let pins = cx.weak_entity();
        let name = folder.name.clone();
        let toast = Toast::new(Outcome::Done, "toast-pin-folder-deleted")
            .name(name)
            .undo(move |cx| {
                pins.update(cx, |this, cx| {
                    this.settings.update(cx, |settings, cx| {
                        settings.restore_pin_folder(folder.clone(), slot, &held, cx)
                    });
                    this.changed(cx);
                })
                .ok();
            });
        Toasts::post(toast, cx);
    }

    /// Opens a closed folder or closes an open one.
    pub fn flip_folder(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(open) = self
            .settings
            .read(cx)
            .pin_folders()
            .iter()
            .find(|folder| folder.id == id)
            .map(|folder| folder.open)
        else {
            return;
        };
        self.settings.update(cx, |settings, cx| {
            settings.set_pin_folder_open(id, !open, cx)
        });
        self.changed(cx);
    }

    /// Unpins and reports it in a toast whose Undo pins it back into the same slot.
    pub fn unpin(&mut self, pin: Pin, cx: &mut Context<Self>) {
        let Some(slug) = self.session.read(cx).slug_for(&pin.id) else {
            return;
        };
        let parted = self.part(slug, &pin, cx);
        let folder = self.folder_of(&pin, cx);
        self.settings
            .update(cx, |settings, cx| settings.unpin(slug, &pin, cx));
        self.tell(&pin, false, folder, cx);
        self.changed(cx);

        let Some(parted) = parted else {
            return;
        };
        let pins = cx.weak_entity();
        let back = parted.clone();
        let toast = Toast::new(Outcome::Done, "toast-unpinned")
            .name(parted.pin.label())
            .target(target(&parted.pin))
            .undo(move |cx| {
                pins.update(cx, |this, cx| this.restore(back.clone(), cx))
                    .ok();
            });
        self.unpinned = Some((Toasts::post(toast, cx), parted.pin));
    }

    /// The stored pin matching `pin` with its folder and the slot it holds there, or `None`
    /// when it is not pinned.
    fn part(&self, slug: &'static str, pin: &Pin, cx: &App) -> Option<Parted> {
        let slugs = self.session.read(cx).active_slugs();
        let filed = self.settings.read(cx).filed(&slugs);
        let (folder, stored) = filed.iter().find(|(_, held)| held.same(pin))?.clone();
        let gap = filed
            .iter()
            .filter(|(home, _)| *home == folder)
            .position(|(_, held)| held.same(pin))?;
        Some(Parted {
            slug,
            pin: stored,
            folder,
            gap,
        })
    }

    /// Pins a parted pin back into its slot, as an Undo does. Nothing happens when it is pinned
    /// already or its provider is no longer the live one.
    fn restore(&mut self, parted: Parted, cx: &mut Context<Self>) {
        let Parted {
            slug,
            pin,
            folder,
            gap,
        } = parted;
        if self.holds(&pin, cx) {
            return;
        }
        let session = self.session.read(cx);
        let slugs = session.active_slugs();
        if session.slug_for(&pin.id) != Some(slug) || !slugs.contains(&slug) {
            return;
        }
        self.settings.update(cx, |settings, cx| {
            settings.pin(slug, pin.clone(), folder, Some(gap), &slugs, cx)
        });
        self.tell(&pin, true, folder, cx);
        self.changed(cx);
    }

    /// Mirrors a change into the provider's own pins. Nothing happens for a provider that keeps
    /// none, or for an item it does not carry in its library. `folder` is where the pin goes back
    /// to when the provider refuses an unpin.
    fn tell(&mut self, pin: &Pin, pinned: bool, folder: Option<u64>, cx: &mut Context<Self>) {
        let Some(uri) = self.remote(pin, cx) else {
            return;
        };
        let mine = cx.entity();
        let pin = pin.clone();
        self.library.update(cx, |library, cx| {
            library.set_pinned(
                uri,
                pinned,
                move |kept, cx| {
                    if kept {
                        return;
                    }
                    mine.update(cx, |this, cx| this.revert(pin, pinned, folder, cx));
                },
                cx,
            );
        });
    }

    /// Freezes the list the way it reads on screen and goes back to the dragged order, so a slot
    /// the user aimed at means the same thing in the store as it did on screen.
    fn settle(&mut self, cx: &mut Context<Self>) {
        if !self.sorted(cx) {
            return;
        }
        let (folders, loose) = self.grouped(cx);
        let order: Vec<u64> = folders.iter().map(|(folder, _)| folder.id).collect();
        let shown: Vec<Pin> = folders
            .into_iter()
            .flat_map(|(_, pins)| pins)
            .chain(loose)
            .collect();
        let slugs = self.session.read(cx).active_slugs();
        self.settings.update(cx, |settings, cx| {
            settings.rearrange(&shown, &slugs, cx);
            settings.order_pin_folders(&order, cx);
            settings.set_sidebar_pin_sort(None, false, cx);
        });
        self.forget();
    }

    /// Puts the local list back after the provider refused the change, and takes down the
    /// toast that reported an unpin the provider turned away.
    fn revert(&mut self, pin: Pin, attempted: bool, folder: Option<u64>, cx: &mut Context<Self>) {
        let Some(slug) = self.session.read(cx).slug_for(&pin.id) else {
            return;
        };
        if !attempted && let Some((toast, unpinned)) = self.unpinned.take() {
            match unpinned.same(&pin) {
                true => Toasts::close(toast, cx),
                false => self.unpinned = Some((toast, unpinned)),
            }
        }
        let slugs = self.session.read(cx).active_slugs();
        self.settings.update(cx, |settings, cx| match attempted {
            true => settings.unpin(slug, &pin, cx),
            false => settings.pin(slug, pin, folder, None, &slugs, cx),
        });
        self.changed(cx);
    }

    /// The provider's own uri for a pin, when the provider keeps pins itself. A listed item
    /// keeps the uri it was listed with, and the provider builds one for anything else. A
    /// provider that keeps no pins, as YouTube does, answers `None`, so the pin stays a local
    /// one rather than failing on a call it cannot make.
    fn remote(&self, pin: &Pin, cx: &App) -> Option<String> {
        let session = self.session.read(cx);
        if !session.capabilities().pins || music::is_local_id(&pin.id) {
            return None;
        }
        let listed = self
            .library
            .read(cx)
            .pin_targets()?
            .iter()
            .find(|item| pin_of(item).is_some_and(|listed| listed.same(pin)))
            .map(|item| item.uri.clone());
        listed.or_else(|| {
            session
                .client_of(Shelf::Streaming)?
                .pin_uri(target_kind(pin.kind)?, &pin.id)
        })
    }

    /// Follows the provider's own pins: one it pinned elsewhere joins the local list, one it
    /// dropped elsewhere leaves it. Only a change since the last look counts, so a local pin the
    /// provider never held stays put, and the first look after signing in imports what is there.
    /// A dropped pin is found in `mirrored`, since a provider may stop listing it altogether.
    fn absorb(&mut self, cx: &mut Context<Self>) {
        if self.library.read(cx).pin_pending() {
            return;
        }
        let Some(items) = self.library.read(cx).pin_targets() else {
            self.mirrored.clear();
            return;
        };
        let now: Vec<(String, Pin)> = items
            .iter()
            .filter(|item| item.pinned)
            .filter_map(|item| Some((item.uri.clone(), pin_of(item)?)))
            .collect();
        if now.len() == self.mirrored.len()
            && now.iter().all(|(uri, _)| self.mirrored.contains_key(uri))
        {
            return;
        }
        let session = self.session.read(cx);
        let slugs = session.active_slugs();
        let held = self.settings.read(cx).pinned(&slugs);
        let known = |pin: &Pin| held.iter().any(|known| known.same(pin));
        let arrived: Vec<_> = now
            .iter()
            .filter(|(uri, pin)| !self.mirrored.contains_key(uri) && !known(pin))
            .filter_map(|(uri, pin)| Some((session.slug_for(uri)?, pin.clone())))
            .collect();
        let left: Vec<_> = self
            .mirrored
            .iter()
            .filter(|(uri, pin)| !now.iter().any(|(held, _)| held == *uri) && known(pin))
            .filter_map(|(uri, pin)| Some((session.slug_for(uri)?, pin.clone())))
            .collect();
        self.mirrored = now.into_iter().collect();
        if arrived.is_empty() && left.is_empty() {
            return;
        }
        self.settings.update(cx, |settings, cx| {
            for (slug, pin) in left {
                settings.unpin(slug, &pin, cx);
            }
            for (slug, pin) in arrived {
                settings.pin(slug, pin, None, None, &slugs, cx);
            }
        });
        self.changed(cx);
    }
}

/// `pins` as rows filed under `folder`, each knowing its slot there.
fn listed(pins: Vec<Pin>, folder: Option<u64>) -> impl Iterator<Item = PinRow> {
    let count = pins.len();
    pins.into_iter()
        .enumerate()
        .map(move |(slot, pin)| PinRow::Pin {
            pin,
            folder,
            slot,
            last: slot + 1 == count,
        })
}

/// The page a pin's toast links its name to.
fn target(pin: &Pin) -> Option<Target> {
    let id = pin.id.clone().into();
    Some(match pin.kind {
        PinKind::Liked => return None,
        PinKind::Playlist => Target::Playlist(id),
        PinKind::Album => Target::Album(id),
        PinKind::Artist => Target::Artist(id),
        PinKind::Song => Target::Song(id),
    })
}

fn rank(kind: PinKind) -> u8 {
    match kind {
        PinKind::Liked => 0,
        PinKind::Playlist => 1,
        PinKind::Album => 2,
        PinKind::Artist => 3,
        PinKind::Song => 4,
    }
}

/// The pin a provider's pin target stands for, or `None` for a row Sonora cannot open on its own,
/// such as a folder or a podcast.
fn pin_of(item: &PinTarget) -> Option<Pin> {
    let kind = match item.kind {
        PinTargetKind::LikedSongs => return Some(Pin::liked().cover(item.cover.clone())),
        PinTargetKind::Playlist => PinKind::Playlist,
        PinTargetKind::Album => PinKind::Album,
        PinTargetKind::Artist => PinKind::Artist,
        _ => return None,
    };
    let (_, id) = item.uri.rsplit_once(':')?;

    Some(Pin::new(kind, id, item.name.clone()).cover(item.cover.clone()))
}

/// The provider's kind for a pin, or `None` for a song, which no provider pins.
fn target_kind(kind: PinKind) -> Option<PinTargetKind> {
    match kind {
        PinKind::Playlist => Some(PinTargetKind::Playlist),
        PinKind::Album => Some(PinTargetKind::Album),
        PinKind::Artist => Some(PinTargetKind::Artist),
        PinKind::Liked => Some(PinTargetKind::LikedSongs),
        PinKind::Song => None,
    }
}

#[cfg(test)]
mod tests {
    use super::pin_of;
    use music::{PinTarget, PinTargetKind};
    use ui::PinKind;

    fn item(uri: &str, kind: PinTargetKind) -> PinTarget {
        PinTarget {
            uri: uri.to_owned(),
            name: "name".into(),
            subtitle: "subtitle".into(),
            cover: None,
            kind,
            pinned: false,
        }
    }

    #[test]
    fn a_library_row_keeps_only_its_bare_id() {
        let pin = pin_of(&item("spotify:album:4aB", PinTargetKind::Album)).unwrap();
        assert_eq!(pin.kind, PinKind::Album);
        assert_eq!(pin.id, "4aB");
        assert!(pin_of(&item("spotify:show:4aB", PinTargetKind::Show)).is_none());
        assert!(pin_of(&item("bare", PinTargetKind::Album)).is_none());
    }
}
