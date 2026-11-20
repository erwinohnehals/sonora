//! The small player that sits on the Windows taskbar, and the playlist picker it opens.
//!
//! Both are windows of their own, which `sonora` opens, places over the taskbar and keeps on
//! top of it. This module only draws them. Each window is too small to show a tooltip in, so
//! the buttons keep their tooltip text as their accessible name and draw none.

use std::cell::Cell;
use std::rc::Rc;

use gpui::accesskit::Role;
use gpui::prelude::*;
use gpui::{
    Bounds, Context, Entity, EventEmitter, MouseButton, Pixels, SharedString, Size, Subscription,
    Window, div, px,
};
use i18n::t;
use music::Track;
use state::{Playback, Sonora};
use ui::{ActiveTheme as _, Artwork, Button, Menu};

use crate::shared::menus::ItemMenu;
use crate::shared::playlist_editor::{Edit, PlaylistEditor};
use crate::shared::transport;

/// The narrowest the capsule still shows the title and artist. Narrower, it keeps the cover and
/// the buttons, since the taskbar has no more room for it.
const TEXT_ROOM: Pixels = px(240.);

/// The capsule's outline, which the cover has to fit inside with the rest.
const BORDER: Pixels = px(1.);

/// The clear room around the picker's panel, so its shadow is not cut off at the window's edge.
const SHADOW_ROOM: Pixels = px(12.);

/// What the taskbar player asks of the window around it.
pub enum TaskbarPlayerEvent {
    /// Open the playlist picker for the playing track, above `anchor`, which is the Add to
    /// playlist button's bounds in the window.
    Pick {
        track: Track,
        anchor: Bounds<Pixels>,
    },
}

/// The capsule on the taskbar: the playing cover, title and artist, the heart, Add to playlist,
/// and previous, play or pause and next.
pub struct TaskbarPlayer {
    playback: Entity<Playback>,
    /// Where the Add to playlist button was last drawn, so the picker opens above it.
    pick_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Whether the picker is open, which the Add to playlist button tells screen readers.
    picking: bool,
}

impl EventEmitter<TaskbarPlayerEvent> for TaskbarPlayer {}

impl TaskbarPlayer {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let sonora = Sonora::global(cx);
        let (playback, library, devices) = (
            sonora.playback.clone(),
            sonora.library.clone(),
            sonora.devices.clone(),
        );
        cx.observe(&playback, |_, _, cx| cx.notify()).detach();
        cx.observe(&library, |_, _, cx| cx.notify()).detach();
        cx.observe(&devices, |_, _, cx| cx.notify()).detach();
        Self {
            playback,
            pick_bounds: Rc::default(),
            picking: false,
        }
    }

    /// Tells the Add to playlist button whether the picker it opened is showing.
    pub fn set_picking(&mut self, picking: bool, cx: &mut Context<Self>) {
        if self.picking != picking {
            self.picking = picking;
            cx.notify();
        }
    }

    fn pick(&mut self, cx: &mut Context<Self>) {
        let Some(track) = self.playback.read(cx).shown_track(cx) else {
            return;
        };
        let Some(anchor) = self.pick_bounds.get() else {
            return;
        };
        cx.emit(TaskbarPlayerEvent::Pick { track, anchor });
    }

    /// The cover, title and artist. A click brings the main window up.
    fn song(
        &self,
        track: Option<&Track>,
        roomy: bool,
        cover_size: Pixels,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let hover = theme.secondary_hover;
        let label = match track {
            Some(track) => SharedString::from(format!("{} — {}", track.name, track.artists)),
            None => t!("player-nothing-playing"),
        };
        let cover = track.and_then(|track| track.cover.clone());
        div()
            .id("taskbar-player-song")
            .role(Role::Button)
            .aria_label(label)
            .flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .gap_2()
            .px_1()
            .rounded(theme.radius)
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
            .on_click(|_, _, cx| state::show_main_window(cx))
            .child(Artwork::new(cover).size(cover_size).flex_none())
            .when(roomy, |this| {
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .line_height(gpui::relative(ui::LEADING))
                        .map(|this| match track {
                            Some(track) => this
                                .child(
                                    div()
                                        .text_size(theme.text(ui::Text::Label))
                                        .text_color(theme.foreground)
                                        .truncate()
                                        .child(track.name.clone()),
                                )
                                .child(
                                    div()
                                        .text_size(theme.text(ui::Text::Small))
                                        .text_color(theme.muted_foreground)
                                        .truncate()
                                        .child(track.artists.clone()),
                                ),
                            None => this.child(
                                div()
                                    .text_size(theme.text(ui::Text::Label))
                                    .text_color(theme.muted_foreground)
                                    .truncate()
                                    .child(t!("player-nothing-playing")),
                            ),
                        }),
                )
            })
    }
}

impl Render for TaskbarPlayer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.set_rem_size(cx.theme().font_size);
        let theme = *cx.theme();
        let track = self.playback.read(cx).shown_track(cx);
        let viewport = window.viewport_size();
        let roomy = viewport.width >= TEXT_ROOM;
        // A large text size grows the cover past what a taskbar row holds, so it stops there.
        let cover_size = theme
            .metrics
            .control
            .min(viewport.height - theme.metrics.pad - BORDER * 2.);
        let playable = track.as_ref().is_some_and(|track| track.id.is_some());
        let bounds = self.pick_bounds.clone();

        let add = Button::new("taskbar-add-to-playlist")
            .ghost()
            .backgroundless()
            .small()
            .icon("icons/square-plus.svg")
            .tint(theme.muted_foreground)
            .tooltip_above("menu-add-to-playlist")
            .tipless()
            .expanded(self.picking)
            .disabled(!playable)
            .on_click(cx.listener(|this, _, _, cx| this.pick(cx)));

        div()
            .size_full()
            .flex()
            .items_center()
            .p(theme.metrics.pad / 2.)
            .child(
                div()
                    .id("taskbar-player")
                    .role(Role::Group)
                    .aria_label(t!("taskbar-player"))
                    .size_full()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px(theme.metrics.pad / 2.)
                    .rounded(theme.radius)
                    .border(BORDER)
                    .border_color(theme.border)
                    .bg(theme.popover)
                    .text_color(theme.popover_foreground)
                    // A right click on the taskbar belongs to the taskbar; here it does nothing
                    // rather than reaching the main window's menus.
                    .on_mouse_down(MouseButton::Right, |_, window, _| window.prevent_default())
                    .child(self.song(track.as_ref(), roomy, cover_size, cx))
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .child(transport::like(track.as_ref(), &self.playback, cx).tipless())
                            .child(
                                div()
                                    .on_children_prepainted(move |children, _, _| {
                                        bounds.set(children.first().copied());
                                    })
                                    .child(add),
                            )
                            .child(transport::previous(&self.playback, false, cx).tipless())
                            .child(transport::toggle(&self.playback, false, false, cx).tipless())
                            .child(transport::next(&self.playback, false, cx).tipless()),
                    ),
            )
    }
}

/// What the playlist picker asks of the window around it.
pub enum PlaylistPickerEvent {
    /// The panel measured so the window has to be `size`, which leaves `margin` clear around
    /// the panel for its shadow. The window goes `margin` lower than the panel's own edge.
    Fit { size: Size<Pixels>, margin: Pixels },
    /// The picker is done: Escape, or a click away from it.
    Close,
}

/// The playlists the playing track can go into, as a panel filling a window of its own above
/// the taskbar player. A pick adds the track, or takes it out of a playlist that holds it, and
/// the panel stays open for the next one.
pub struct PlaylistPicker {
    track: Track,
    menus: ItemMenu,
    /// The size last reported, so a frame that measures the same asks nothing of the window.
    fitted: Option<Size<Pixels>>,
    /// Whether the window has been active yet. One the system never let come to the front
    /// stays open until the Add to playlist button closes it, rather than closing unseen.
    was_active: bool,
    _activation: Subscription,
}

impl EventEmitter<PlaylistPickerEvent> for PlaylistPicker {}

impl PlaylistPicker {
    pub fn new(track: Track, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let library = Sonora::global(cx).library.clone();
        cx.observe(&library, |_, _, cx| cx.notify()).detach();
        let me = cx.entity_id();
        let scrollbar = cx.new(|_| ui::Scrollbar::inset().watching(me));
        // The picker is a menu, so it closes the moment focus goes anywhere else.
        let _activation = cx.observe_window_activation(window, |this, window, cx| {
            match window.is_window_active() {
                true => this.was_active = true,
                false if this.was_active => cx.emit(PlaylistPickerEvent::Close),
                false => {}
            }
        });
        Self {
            track,
            menus: ItemMenu::new(scrollbar, cx),
            fitted: None,
            was_active: window.is_window_active(),
            _activation,
        }
    }

    fn measured(&mut self, panel: Size<Pixels>, cx: &mut Context<Self>) {
        let size = Size {
            width: panel.width + SHADOW_ROOM * 2.,
            height: panel.height + SHADOW_ROOM * 2.,
        };
        let same = self.fitted.is_some_and(|fitted| {
            (fitted.width - size.width).abs() < px(1.)
                && (fitted.height - size.height).abs() < px(1.)
        });
        if same {
            return;
        }
        self.fitted = Some(size);
        cx.emit(PlaylistPickerEvent::Fit {
            size,
            margin: SHADOW_ROOM,
        });
    }
}

impl Render for PlaylistPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.set_rem_size(cx.theme().font_size);
        let menu: Menu = self
            .menus
            .picker(&self.track, create_in_main_window, cx)
            .name(t!("menu-add-to-playlist"))
            .inline()
            .top(SHADOW_ROOM)
            .left(SHADOW_ROOM);
        let this = cx.entity().downgrade();

        div()
            .size_full()
            .relative()
            .capture_key_down(cx.listener(|_, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    cx.emit(PlaylistPickerEvent::Close);
                }
            }))
            .on_children_prepainted(move |children, _, cx| {
                let Some(panel) = children.first() else {
                    return;
                };
                let (size, this) = (panel.size, this.clone());
                // Measured mid-paint, so the window is fitted once the frame is done.
                cx.defer(move |cx| {
                    this.update(cx, |this, cx| this.measured(size, cx)).ok();
                });
            })
            .child(menu)
    }
}

/// Opens the editor for a new playlist in the main window, bringing it up first, since the
/// picker is far too small to hold it.
fn create_in_main_window(edit: Edit, _: &mut Window, cx: &mut gpui::App) {
    state::show_main_window(cx);
    cx.defer(move |cx| {
        let Some(main) = state::main_windows(cx).first().copied() else {
            return;
        };
        main.update(cx, |_, window, cx| PlaylistEditor::open(edit, window, cx))
            .ok();
    });
}
