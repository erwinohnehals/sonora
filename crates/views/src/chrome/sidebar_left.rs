use std::rc::Rc;

use ui::{
    ActiveTheme as _, Button, Card, Deck, DraggedPin, Edge, Menu, MenuItem, Panel, Picker, Pin,
    Pinnable as _, Popup, SNUG, Scroller, Shield, Side, Spot, SubmenuState, Tabs, Text, Vacancy,
    drop_gap, drop_marker,
};

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, DragMoveEvent, ElementId, Entity, Hsla, MouseButton, MouseDownEvent,
    Pixels, Point, Render, ScrollHandle, svg,
};
use gpui::{Window, div, px};
use i18n::t;
use router::{
    Destination, LibraryTab, NavEntry, Navigation, NavigationEvent, SettingsTab, navigate,
};
use state::{
    AppSettings, Library, Origin, PinFolder, PinRow, PinSort, Pins, Playback, Session, Shelf,
    Sonora,
};

use crate::shared::menus::{ItemMenu, item_menu};
use crate::shared::playlist_editor::{Edit, PlaylistEditor};

/// The one drag list the pinned section keeps, so a pin dropped anywhere in it lands in order.
const PINS: &str = "sidebar-pins";

const NAV: [(Option<NavEntry>, &str, Destination); 6] = [
    (Some(NavEntry::Home), "icons/house.svg", Destination::Home),
    (
        Some(NavEntry::Search),
        "icons/search.svg",
        Destination::Search,
    ),
    (
        Some(NavEntry::Library),
        "icons/library-big.svg",
        Destination::Library(LibraryTab::Songs),
    ),
    (
        Some(NavEntry::Local),
        "icons/file-music.svg",
        Destination::Local(LibraryTab::Songs),
    ),
    (
        Some(NavEntry::History),
        "icons/rotate-ccw-clock.svg",
        Destination::History,
    ),
    (
        None,
        "icons/settings.svg",
        Destination::Settings(SettingsTab::General),
    ),
];

const LIBRARY_TABS: [(&str, LibraryTab); 4] = [
    ("nav-songs", LibraryTab::Songs),
    ("nav-albums", LibraryTab::Albums),
    ("nav-artists", LibraryTab::Artists),
    ("nav-playlists", LibraryTab::Playlists),
];

const MIN_WIDTH: Pixels = px(160.);
const MAX_WIDTH: Pixels = px(400.);
const HINT_HEIGHT: Pixels = px(42.);
const VACANCY_HEIGHT: Pixels = px(88.);
/// How far the pin mark on a library row falls back from the accent.
const PIN_MARK: f32 = 0.7;
/// The space between two pinned entries, the same as the `gap_1` between the rows above them.
const ROW_GAP: Pixels = px(4.);
/// How many pads a folder's pins sit in from the folder.
const NEST: f32 = 2.;
const FOLDER_MENU: Pixels = px(220.);

/// The two navigation entries that expand into tabs rather than navigate.
#[derive(Clone, Copy, PartialEq)]
enum Group {
    Library,
    Local,
}

impl Group {
    fn of(destination: &Destination) -> Option<Self> {
        match destination {
            Destination::Library(_) => Some(Self::Library),
            Destination::Local(_) => Some(Self::Local),
            _ => None,
        }
    }
}

/// Where a pin dragged over the pinned section would land.
#[derive(Clone, Copy, PartialEq)]
enum Aim {
    /// At the end of a folder, aimed at from the folder's own row.
    Into(u64),
    /// At a slot among the pins of a folder, or among the pins in none.
    Gap { folder: Option<u64>, gap: usize },
}

/// Where an entry of the pinned section sits, which decides where a pin dropped on it lands.
#[derive(Clone, Copy)]
enum Seat {
    /// A pin: its folder, its slot there, and whether it is the last one there. `tail` marks
    /// the last pinned row of all while no pin sits outside a folder, since a drop among those
    /// pins is then drawn below it.
    Held {
        folder: Option<u64>,
        slot: usize,
        last: bool,
        tail: bool,
    },
    /// The library under the pins. A drop lands after the `loose` pins that are in no folder.
    Library { loose: usize },
}

/// What a right click in the pinned section opened a menu on.
#[derive(Clone)]
enum Target {
    /// A pin, or an entry of the library under the pins when `held` is false.
    Pin {
        pin: Pin,
        held: bool,
    },
    Folder(PinFolder),
}

pub(crate) struct SidebarLeft {
    settings: Entity<AppSettings>,
    session: Entity<Session>,
    trail: Entity<Navigation>,
    at: Destination,
    width: Pixels,
    open: bool,
    cramped: bool,
    forced: Option<bool>,
    library_open: bool,
    local_open: bool,
    pinned_open: bool,
    dropping: bool,
    aim: Option<Aim>,
    playback: Entity<Playback>,
    pins: Entity<Pins>,
    library: Entity<Library>,
    track_menu: ItemMenu,
    folder_submenu: SubmenuState,
    context_menu: Option<(Target, Point<Pixels>)>,
    scrollbar: Entity<ui::Scrollbar>,
    popovers: ui::Popovers,
}

impl SidebarLeft {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let settings = Sonora::global(cx).settings.clone();
        let session = Sonora::global(cx).session.clone();
        let playback = Sonora::global(cx).playback.clone();
        let pins = Sonora::global(cx).pins.clone();
        let library = Sonora::global(cx).library.clone();
        cx.observe(&library, |_, _, cx| cx.notify()).detach();
        cx.observe(&pins, |_, _, cx| cx.notify()).detach();
        cx.observe(&playback, |_, _, cx| cx.notify()).detach();
        let me = cx.entity_id();
        let playlist_scrollbar = cx.new(|_| ui::Scrollbar::inset().watching(me));
        let scrollbar = cx.new(|_| ui::Scrollbar::new(ScrollHandle::new()).watching(me));
        let width = px(settings.read(cx).sidebar_width()).clamp(MIN_WIDTH, MAX_WIDTH);
        let open = settings.read(cx).sidebar_open();
        let pinned_open = settings.read(cx).sidebar_pinned_open();
        let trail = router::trail(cx);

        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        cx.observe(&settings, |_, _, cx| cx.notify()).detach();
        cx.observe(&trail, |_, _, cx| cx.notify()).detach();
        cx.subscribe(&trail, |this, _, _: &NavigationEvent, cx| {
            this.dismiss(cx);
            cx.notify();
        })
        .detach();

        let at = trail.read(cx).current();
        let library_open = matches!(at, Destination::Library(_));
        let local_open = matches!(at, Destination::Local(_));

        Self {
            settings,
            session,
            trail,
            at,
            width,
            open,
            forced: None,
            cramped: false,
            library_open,
            local_open,
            pinned_open,
            dropping: false,
            aim: None,
            playback,
            pins,
            library,
            track_menu: ItemMenu::new(playlist_scrollbar, cx),
            folder_submenu: SubmenuState::default(),
            context_menu: None,
            scrollbar,
            popovers: ui::Popovers::default(),
        }
    }

    fn follow(&mut self, current: &Destination) {
        if self.at == *current {
            return;
        }
        self.at = current.clone();
        let (library, local) = expanded(current);
        self.library_open |= library;
        self.local_open |= local;
    }

    fn dismiss_menu(&mut self, cx: &mut Context<Self>) {
        self.track_menu.reset(cx);
        self.folder_submenu.reset();
        self.context_menu = None;
        cx.notify();
    }

    /// Opens a context menu on `target` where the pointer went down.
    fn summon(&mut self, target: Target, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.track_menu.reset(cx);
        self.folder_submenu.reset();
        self.context_menu = Some((target, position));
        cx.notify();
    }

    fn menu(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (target, position) = self.context_menu.clone()?;
        let menu = match target {
            Target::Pin { pin, held } => {
                let menu = item_menu(&pin, &self.track_menu, self.playback.clone(), cx);
                match held {
                    true => menu.item(self.folder_item(&pin, cx)),
                    false => menu,
                }
            }
            Target::Folder(folder) => folder_menu(folder, self.pins.clone()),
        };

        Some(
            Popup::new(position, menu)
                .on_close(cx.listener(|this, _, _, cx| this.dismiss_menu(cx))),
        )
    }

    /// The Move to folder entry of a pin's menu: New folder, then every folder with the one
    /// holding the pin checked, then a way back out of it.
    fn folder_item(&self, pin: &Pin, cx: &App) -> MenuItem {
        let current = self.pins.read(cx).folder_of(pin, cx);
        let folders = self.pins.read(cx).folders(cx);
        let fresh = pin.clone();
        let freed = pin.clone();
        let pins = self.pins.clone();

        let menu = Menu::new("pin-folder-submenu")
            .w(FOLDER_MENU)
            .item(
                MenuItem::new("new-pin-folder", t!("nav-new-folder"))
                    .icon("icons/folder-plus.svg")
                    .on_click(move |_, window, cx| {
                        PlaylistEditor::open(Edit::CreateFolder(Some(fresh.clone())), window, cx);
                    }),
            )
            .items(
                (!folders.is_empty())
                    .then(|| MenuItem::separator("pin-folder-separator"))
                    .into_iter()
                    .chain(folders.into_iter().map(|folder| {
                        let pins = pins.clone();
                        let pin = pin.clone();
                        let id = folder.id;

                        MenuItem::new(format!("pin-folder-{id}"), folder.name)
                            .icon("icons/folder.svg")
                            .checked(current == Some(id))
                            .on_click(move |_, _, cx| {
                                pins.update(cx, |pins, cx| pins.file(pin.clone(), Some(id), cx));
                            })
                    })),
            )
            .items(current.map(|_| {
                MenuItem::new("pin-folder-out", t!("nav-move-out-of-folder"))
                    .icon("icons/undo-2.svg")
                    .on_click(move |_, _, cx| {
                        pins.update(cx, |pins, cx| pins.file(freed.clone(), None, cx));
                    })
            }));

        MenuItem::new("move-to-folder", t!("nav-move-to-folder"))
            .icon("icons/folder.svg")
            .submenu(menu, self.folder_submenu.clone())
    }

    pub fn is_open(&self) -> bool {
        self.forced.unwrap_or(self.open && !self.cramped)
    }

    pub fn overlays(&self) -> bool {
        self.cramped && self.is_open()
    }

    pub fn overlay_width(&self) -> Pixels {
        match self.overlays() {
            true => self.width,
            false => Pixels::ZERO,
        }
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        if !self.overlays() {
            return;
        }
        self.forced = Some(false);
        cx.notify();
    }

    pub fn occupied_width(&self) -> Pixels {
        match self.is_open() && !self.overlays() {
            true => self.width,
            false => Pixels::ZERO,
        }
    }

    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        match self.cramped {
            true => self.forced = Some(!self.is_open()),
            false => {
                self.open = !self.open;
                self.persist(cx);
            }
        }
        cx.notify();
    }

    fn ceiling(&self, window: &Window, cx: &Context<Self>) -> Pixels {
        let reserved = match self.overlays() {
            true => Pixels::ZERO,
            false => SNUG + super::Chrome::reserved_right(cx),
        };

        super::cap(MIN_WIDTH, MAX_WIDTH, reserved, window)
    }

    /// Flips into or out of the cramped state from the room the window leaves
    /// once `right` pixels are held back for the right sidebar. This runs inside
    /// a render, where a notify schedules nothing, so a flip asks for a full
    /// window refresh instead. That effect lands once the draw is over.
    pub fn adapt(&mut self, right: Pixels, window: &Window, cx: &mut App) {
        self.width = ui::snapped(self.width, window);

        let space_left = window.viewport_size().width - self.width - right;
        let cramped = space_left < SNUG;
        if cramped != self.cramped {
            self.cramped = cramped;
            self.forced = None;
            cx.refresh_windows();
        }
    }

    /// The navigation entries, each followed by its tabs while its group is open.
    fn navigation(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        // A shelf with rows on it belongs in the sidebar even before the session has restored,
        // since a snapshot fills it while the provider is still answering.
        let stocked = self.session.read(cx).authenticated()
            || self.library.read(cx).stocked(Shelf::Streaming);
        let mut rows = Vec::new();
        for (index, (entry, _, destination)) in NAV.iter().enumerate() {
            if entry.is_some_and(|entry| !self.settings.read(cx).nav_shown(entry.id())) {
                continue;
            }
            let group = Group::of(destination);
            if group == Some(Group::Library) && !stocked {
                continue;
            }
            rows.push(self.nav(index, cx));
            if let Some(group) = group.filter(|group| self.opened(*group)) {
                rows.push(self.tabs(group, cx));
            }
        }
        rows
    }

    /// The pinned section: its header, and its entries once it is expanded. The entries are a
    /// `Deck`, so only the ones on screen are ever built.
    fn pins(&self, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if !self.settings.read(cx).nav_shown(NavEntry::Pins.id()) {
            return Vec::new();
        }

        let mut rows = vec![self.pins_header(cx)];
        if !self.pinned_open {
            return rows;
        }

        let pins = self.pins.read(cx);
        let pinned = pins.rows(cx);
        let rest = match self.settings.read(cx).sidebar_full_library() {
            true => pins.library(cx),
            false => Rc::new(Vec::new()),
        };
        if pinned.is_empty() && rest.is_empty() {
            rows.push(match self.dropping {
                true => hint(cx),
                false => vacancy(),
            });
            return rows;
        }

        let held = pinned.len();
        let loose = pinned
            .iter()
            .filter(|row| matches!(row, PinRow::Pin { folder: None, .. }))
            .count();
        let count = held + rest.len();
        let row = ui::snapped(cx.theme().metrics.list_row, window);
        rows.push(
            Deck::new("sidebar-pins-deck")
                .rows((0..count).map(|_| row))
                .gap(ROW_GAP)
                .draw(cx.processor(move |this, index: usize, _, cx| {
                    if index >= held {
                        return match rest.get(index - held) {
                            Some(pin) => {
                                this.pin_row(index, pin.clone(), Seat::Library { loose }, cx)
                            }
                            None => div().into_any_element(),
                        };
                    }
                    let tail = loose == 0 && index + 1 == held;
                    match pinned.get(index).cloned() {
                        Some(PinRow::Folder { folder, count }) => {
                            this.folder_row(index, folder, count, tail, cx)
                        }
                        Some(PinRow::Pin {
                            pin,
                            folder,
                            slot,
                            last,
                        }) => {
                            let seat = Seat::Held {
                                folder,
                                slot,
                                last,
                                tail,
                            };
                            this.pin_row(index, pin, seat, cx)
                        }
                        None => div().into_any_element(),
                    }
                }))
                .into_any_element(),
        );
        rows
    }

    fn opened(&self, group: Group) -> bool {
        match group {
            Group::Library => self.library_open,
            Group::Local => self.local_open,
        }
    }

    fn flip(&mut self, group: Group) {
        let open = match group {
            Group::Library => &mut self.library_open,
            Group::Local => &mut self.local_open,
        };
        *open = !*open;
    }

    fn pins_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.theme();
        let open = self.pinned_open;

        div()
            .id("sidebar-pins-header")
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .w_full()
            .min_w_0()
            .h(theme.metrics.control_small)
            .pl_2()
            // Narrower on the right than on the left, so the sort glyph falls on the same
            // line as the chevrons of the rows above.
            .pr_1()
            .mt(theme.metrics.pad)
            .mb(px(2.))
            .child(
                div()
                    .id("sidebar-pins-toggle")
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap_1()
                    .cursor_pointer()
                    .child(
                        svg()
                            .path(icons::path(chevron(open)))
                            .flex_none()
                            .size(theme.text(Text::Small))
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        ui::eyebrow(i18n::lookup("nav-pinned", None), cx)
                            .min_w_0()
                            .truncate(),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.pinned_open = !this.pinned_open;
                        let open = this.pinned_open;
                        this.settings.update(cx, |settings, cx| {
                            settings.set_sidebar_pinned_open(open, cx)
                        });
                        cx.notify();
                    })),
            )
            .when(open, |header| header.child(self.sort_picker(cx)))
            // Dragging over the header aims at the top of the pins in no folder, which is
            // otherwise out of reach above the first entry, and is the way out of a folder
            // when every pin is in one.
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<DraggedPin>, _, cx| {
                    let top = Some(Aim::Gap {
                        folder: None,
                        gap: 0,
                    });
                    if !event.bounds.contains(&event.event.position) || this.aim == top {
                        return;
                    }
                    this.aim = top;
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    fn sort_picker(&self, cx: &App) -> AnyElement {
        let chosen = self.pins.read(cx).sort(cx);
        let reversed = self.pins.read(cx).reversed(cx);
        let full = self.settings.read(cx).sidebar_full_library();
        let arrow = match reversed {
            true => "icons/chevron-down.svg",
            false => "icons/chevron-up.svg",
        };
        let pins = self.pins.clone();

        Picker::icon(
            "sidebar-pins-sort",
            &self.popovers,
            "icons/arrow-up-down.svg",
        )
        .tooltip("tool-sort")
        .sticky()
        .width(Picker::NARROW)
        .tint(match self.pins.read(cx).sorted(cx) {
            true => cx.theme().primary,
            false => cx.theme().muted_foreground,
        })
        .items(PinSort::ALL.into_iter().map(move |sort| {
            let pins = pins.clone();

            MenuItem::new(sort.id(), i18n::lookup(sort.key(), None))
                .selected(Some(sort) == chosen)
                .when(Some(sort) == chosen, |item| item.icon(arrow))
                .on_click(move |_, _, cx| {
                    pins.update(cx, |pins, cx| pins.choose(sort, cx));
                })
        }))
        .item(MenuItem::separator("sidebar-pins-scope"))
        .item(
            MenuItem::new(
                "sidebar-full-library",
                i18n::lookup("nav-show-full-library", None),
            )
            .checked(full)
            .on_click({
                let settings = self.settings.clone();
                move |_, _, cx| {
                    settings.update(cx, |settings, cx| {
                        settings.set_sidebar_full_library(!full, cx)
                    });
                }
            }),
        )
        .item(MenuItem::separator("sidebar-pins-folders"))
        .item(
            MenuItem::new("sidebar-new-folder", t!("nav-new-folder"))
                .icon("icons/folder-plus.svg")
                .on_click(|_, window, cx| {
                    PlaylistEditor::open(Edit::CreateFolder(None), window, cx);
                }),
        )
        .into_any_element()
    }

    /// A folder's row. A click opens or shuts it, and a pin dropped on it lands at its end.
    fn folder_row(
        &self,
        index: usize,
        folder: PinFolder,
        count: usize,
        tail: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = *cx.theme();
        let accent = theme.sidebar_accent;
        let id = folder.id;
        let aimed = self.aim == Some(Aim::Into(id));
        let edge = (tail && self.aims_loose()).then_some(Edge::Below);
        let icon = match folder.open {
            true => "icons/folder-open.svg",
            false => "icons/folder.svg",
        };
        let chevron = svg()
            .path(icons::path(chevron(folder.open)))
            .flex_none()
            .size(theme.text(Text::Small))
            .text_color(theme.muted_foreground);
        let named = folder.clone();

        let card = Card::new(("pin-folder", index), folder.name)
            .fallback(icon)
            .tint(theme.muted_foreground)
            .meta(t!("nav-folder-count", count = count))
            .trailing(chevron)
            .when(aimed, |card| card.bg(accent))
            .hover(move |style| style.bg(accent))
            .press(cx.listener(move |this, _, _, cx| {
                this.pins.update(cx, |pins, cx| pins.flip_folder(id, cx));
            }))
            .menu(cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                this.summon(Target::Folder(named.clone()), event.position, cx);
            }))
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<DraggedPin>, _, cx| {
                    let aim = Some(Aim::Into(id));
                    if !event.bounds.contains(&event.event.position) || this.aim == aim {
                        return;
                    }
                    this.aim = aim;
                    cx.notify();
                }),
            );

        seated(index, false, card, edge.map(|edge| (edge, true)), cx)
    }

    /// Whether the drag aims among the pins in no folder.
    fn aims_loose(&self) -> bool {
        matches!(self.aim, Some(Aim::Gap { folder: None, .. }))
    }

    /// One entry: a pin, which carries a drop slot in its folder, or a row of the library
    /// underneath, which can be dragged up into the pins but holds no slot.
    fn pin_row(&self, index: usize, pin: Pin, seat: Seat, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.theme();
        let accent = theme.sidebar_accent;
        let destination = Destination::from(&pin);
        let active = destination == self.trail.read(cx).current();
        let opened = pin.clone();
        let held = matches!(seat, Seat::Held { .. });
        let nested = matches!(
            seat,
            Seat::Held {
                folder: Some(_),
                ..
            }
        );
        // Only worth marking when the library sits alongside, since otherwise every row is a pin.
        let marked = held && self.settings.read(cx).sidebar_full_library();
        // A marker is drawn full width when it stands for the pins in no folder, even on a row
        // inside one, and set in with the row otherwise.
        let marker = match (self.aim, seat) {
            (
                Some(Aim::Gap { folder, gap }),
                Seat::Held {
                    folder: home,
                    slot,
                    last,
                    ..
                },
            ) if folder == home && (gap == slot || (last && gap == slot + 1)) => {
                let edge = match gap == slot {
                    true => Edge::Above,
                    false => Edge::Below,
                };
                Some((edge, false))
            }
            (_, Seat::Held { tail: true, .. }) if self.aims_loose() => Some((Edge::Below, true)),
            _ => None,
        };

        let origin = Origin::from(&pin);
        let playing = self.playback.read(cx).playing_from(&origin, cx) == Some(true);

        let card = Card::new(("pinned", index), pin.label())
            .cover(pin.cover.clone())
            .fallback(pin.kind.icon())
            .when(pin.kind.round(), Card::circle)
            .play(
                playing,
                cx.listener(move |this, _, _, cx| {
                    this.playback
                        .update(cx, |playback, cx| playback.toggle_origin(&origin, cx));
                }),
            )
            .tint(match active {
                true => theme.foreground,
                false => theme.muted_foreground,
            })
            .meta(caption(pin.kind.key(), marked, cx))
            .when(active, |card| card.bg(accent))
            .hover(move |style| style.bg(accent))
            .press(move |_, _, cx| navigate(destination.clone(), cx))
            .menu(cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                let target = Target::Pin {
                    pin: opened.clone(),
                    held,
                };
                this.summon(target, event.position, cx);
            }))
            .when_else(
                held,
                |card| card.pin_from(pin.clone(), Spot::new(PINS, index)),
                |card| card.pin(pin.clone()),
            )
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<DraggedPin>, _, cx| {
                    let slot = match seat {
                        Seat::Held { slot, .. } => slot,
                        Seat::Library { .. } => 0,
                    };
                    let Some(gap) = drop_gap(event.bounds, event.event.position, slot) else {
                        return;
                    };
                    let aim = Some(match seat {
                        Seat::Held { folder, .. } => Aim::Gap { folder, gap },
                        Seat::Library { loose } => Aim::Gap {
                            folder: None,
                            gap: loose,
                        },
                    });
                    if this.aim != aim {
                        this.aim = aim;
                        cx.notify();
                    }
                }),
            );

        seated(index, nested, card, marker, cx)
    }

    /// One navigation entry. An expandable one flips its group open and shut, any other one
    /// navigates and is lit while its section is current.
    fn nav(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.theme();
        let accent = theme.sidebar_accent;
        let (entry, icon, destination) = NAV[index].clone();
        let key = entry.map_or("nav-settings", NavEntry::key);
        let current = self.trail.read(cx).current();
        let group = Group::of(&destination);
        let active = match group {
            Some(group) => Group::of(&current) == Some(group),
            None => destination.same_section(&current),
        };
        let tint = match active {
            true => theme.foreground,
            false => theme.muted_foreground,
        };
        let row = nav_row(index, key, tint, accent).icon(icon);

        match group {
            Some(group) => row
                .trailing(chevron(self.opened(group)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.flip(group);
                    cx.notify();
                })),
            None => row
                .when(active, |button| button.bg(accent))
                .on_click(move |_, _, cx| navigate(destination.clone(), cx)),
        }
        .into_any_element()
    }

    /// The tabs under an expanded group.
    fn tabs(&self, group: Group, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.theme();
        let accent = theme.sidebar_accent;
        let current = self.trail.read(cx).current();
        let tab = |id: ElementId, key: &'static str, destination: Destination| {
            let chosen = destination == current;
            let tint = match chosen {
                true => theme.foreground,
                false => theme.muted_foreground,
            };

            nav_row(id, key, tint, accent)
                .flex_1()
                .when(chosen, |button| button.bg(accent))
                .on_click(move |_, _, cx| navigate(destination.clone(), cx))
        };

        match group {
            Group::Library => Tabs::new().items(
                LIBRARY_TABS
                    .into_iter()
                    .map(|(name, tab_id)| tab(name.into(), name, Destination::Library(tab_id))),
            ),
            Group::Local => Tabs::new().items(LIBRARY_TABS.into_iter().enumerate().map(
                |(slot, (name, tab_id))| {
                    tab(("local-tab", slot).into(), name, Destination::Local(tab_id))
                },
            )),
        }
        .into_any_element()
    }

    fn persist(&self, cx: &mut Context<Self>) {
        let width = self.width / px(1.);
        let open = self.open;
        self.settings
            .update(cx, |settings, cx| settings.set_sidebar(width, open, cx));
    }
}

impl Render for SidebarLeft {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        let sidebar_bg = theme.sidebar;
        let sidebar_border = theme.sidebar_border;

        let current = self.trail.read(cx).current();
        self.follow(&current);
        self.adapt(super::Chrome::reserved_right(cx), window, cx);

        if !cx.has_active_drag() {
            self.dropping = false;
            self.aim = None;
        }

        let mut rows = self.navigation(cx);
        rows.extend(self.pins(window, cx));

        let overlaid = self.overlays();
        let panel = Panel::new("sidebar-left", Side::Left, self.width)
            .limits(MIN_WIDTH, MAX_WIDTH)
            .reach(self.ceiling(window, cx))
            .clears_scrollbar()
            .on_resize(cx.listener(|this, width: &Pixels, _, cx| {
                this.width = *width;
                this.persist(cx);
                cx.notify();
            }))
            .on_drag_move(cx.listener(|this, _: &DragMoveEvent<DraggedPin>, _, cx| {
                if this.dropping {
                    return;
                }
                this.dropping = true;
                // A drag has nowhere to land while the section is closed, so open it.
                if !this.pinned_open {
                    this.pinned_open = true;
                    this.settings.update(cx, |settings, cx| {
                        settings.set_sidebar_pinned_open(true, cx)
                    });
                }
                cx.notify();
            }))
            .on_drop(cx.listener(|this, dragged: &DraggedPin, _, cx| {
                let aim = this.aim.take();
                this.dropping = false;
                let pin = dragged.pin.clone();
                this.pins.update(cx, |pins, cx| match aim {
                    Some(Aim::Into(folder)) => pins.file(pin, Some(folder), cx),
                    Some(Aim::Gap { folder, gap }) => pins.place(pin, folder, Some(gap), cx),
                    None => pins.place(pin, None, None, cx),
                });
                cx.notify();
            }))
            .when(!self.is_open(), |this| this.hidden())
            .when(!theme.transparent, |this| this.bg(sidebar_bg))
            .border_color(sidebar_border)
            .when(overlaid, |this| {
                this.occlude().absolute().left_0().top_0().bottom_0()
            })
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        Scroller::new("sidebar-left-rows", &self.scrollbar)
                            .size_full()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .w_full()
                                    .p_3()
                                    // Leave the last row somewhere to go, clear of the
                                    // button that floats over the bottom.
                                    .pb(ui::perch_room(cx))
                                    .children(rows),
                            ),
                    )
                    .children(ui::return_top("sidebar-return-top", &self.scrollbar, cx)),
            )
            .children(self.menu(cx));

        match overlaid {
            false => panel.into_any_element(),
            true => div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .child(
                    Shield::new("sidebar-shield")
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseDownEvent, _, cx| this.dismiss(cx)),
                        ),
                )
                .child(panel)
                .into_any_element(),
        }
    }
}

/// The kind of an entry, behind a dimmed pin when it is one of the pinned ones.
fn caption(kind: &'static str, marked: bool, cx: &App) -> impl IntoElement {
    let theme = *cx.theme();

    div()
        .flex()
        .items_center()
        .gap(theme.metrics.pad / 4.)
        .min_w_0()
        .when(marked, |row| {
            row.child(
                svg()
                    .path(icons::path("icons/pin.svg"))
                    .flex_none()
                    .size(theme.text(Text::Tiny))
                    .text_color(theme.primary.opacity(PIN_MARK)),
            )
        })
        .child(div().min_w_0().truncate().child(i18n::lookup(kind, None)))
}

/// An entry of the pinned section in its slot, set in when it sits in a folder. `marker` is
/// the drop line to draw on it, and whether it spans the slot's full width rather than the
/// entry's.
fn seated(
    index: usize,
    nested: bool,
    card: Card,
    marker: Option<(Edge, bool)>,
    cx: &App,
) -> AnyElement {
    let theme = *cx.theme();
    let (inner, outer) = match marker {
        Some((edge, true)) => (None, Some(edge)),
        Some((edge, false)) => (Some(edge), None),
        None => (None, None),
    };

    div()
        .id(("pinned-slot", index))
        .relative()
        .flex_none()
        .w_full()
        .min_w_0()
        .when(nested, |slot| slot.pl(theme.metrics.pad * NEST))
        .child(
            div()
                .relative()
                .w_full()
                .min_w_0()
                .child(card)
                .when_some(inner, |entry, edge| entry.child(drop_marker(edge, cx))),
        )
        .when_some(outer, |slot, edge| slot.child(drop_marker(edge, cx)))
        .into_any_element()
}

/// A folder's own menu: rename it, or delete it and leave its pins pinned.
fn folder_menu(folder: PinFolder, pins: Entity<Pins>) -> Menu {
    let id = folder.id;

    Menu::new("pin-folder-menu")
        .item(
            MenuItem::new("rename-pin-folder", t!("nav-rename-folder"))
                .icon("icons/pencil.svg")
                .on_click(move |_, window, cx| {
                    PlaylistEditor::open(Edit::RenameFolder(folder.clone()), window, cx);
                }),
        )
        .item(
            MenuItem::new("delete-pin-folder", t!("nav-delete-folder"))
                .icon("icons/trash-2.svg")
                .tooltip("nav-delete-folder-tip")
                .on_click(move |_, _, cx| {
                    pins.update(cx, |pins, cx| pins.remove_folder(id, cx));
                }),
        )
}

fn hint(cx: &App) -> AnyElement {
    let theme = *cx.theme();

    div()
        .flex()
        .flex_none()
        .min_w_0()
        .items_center()
        .justify_center()
        // A narrow sidebar wraps the line, so the box grows rather than the text spilling out.
        .min_h(HINT_HEIGHT)
        .mx_2()
        .px_2()
        .py_1()
        .rounded(theme.radius)
        .border_1()
        .border_dashed()
        .border_color(theme.sidebar_border)
        .text_size(theme.text(Text::Small))
        .text_color(theme.muted_foreground)
        .text_center()
        .child(div().min_w_0().child(i18n::lookup("nav-pin-hint", None)))
        .into_any_element()
}

fn vacancy() -> AnyElement {
    Vacancy::new(i18n::lookup("nav-nothing-pinned", None))
        .icon("icons/pin.svg")
        .compact()
        .flex_none()
        .h(VACANCY_HEIGHT)
        .px_2()
        .into_any_element()
}

/// Which groups the route opens: Your Library and Local Music, in that order.
fn expanded(current: &Destination) -> (bool, bool) {
    (
        matches!(current, Destination::Library(_)),
        matches!(current, Destination::Local(_)),
    )
}

fn chevron(open: bool) -> &'static str {
    match open {
        true => "icons/chevron-down.svg",
        false => "icons/chevron-right.svg",
    }
}

fn nav_row(id: impl Into<ElementId>, key: &'static str, tint: Hsla, accent: Hsla) -> Button {
    Button::new(id)
        .ghost()
        .label(i18n::lookup(key, None))
        .tint(tint)
        .gap_2p5()
        .justify_start()
        .hover(move |style| style.bg(accent))
        .active(move |style| style.bg(accent))
}

#[cfg(test)]
mod tests {
    use router::{Destination, LibraryTab, SettingsTab};

    use super::expanded;

    #[test]
    fn a_section_expands_only_where_it_leads() {
        assert_eq!(
            expanded(&Destination::Library(LibraryTab::Albums)),
            (true, false)
        );
        assert_eq!(
            expanded(&Destination::Local(LibraryTab::Albums)),
            (false, true)
        );
        assert_eq!(
            expanded(&Destination::Settings(SettingsTab::General)),
            (false, false)
        );
    }

    #[test]
    fn content_belongs_to_neither_section() {
        let away = [
            Destination::Home,
            Destination::Search,
            Destination::Album("id".into()),
            Destination::Playlist("id".into()),
            Destination::Artist("id".into()),
            Destination::Song("id".into()),
        ];

        for destination in away {
            assert_eq!(expanded(&destination), (false, false), "{destination:?}");
        }
    }
}
