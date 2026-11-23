//! The small player on the Windows taskbar, and the playlist picker it opens.
//!
//! [`views::TaskbarPlayer`] draws the player and [`views::PlaylistPicker`] the picker; this module
//! opens their windows, lays the player over a free stretch of the taskbar, and keeps it there as
//! the taskbar changes. It starts out at the left end, and dragging it moves it to any other free
//! place. The Win32 side of that lives in [`shell`]. The player follows the `taskbar_player`
//! setting and is off by default.
//!
//! Moving, showing or raising a window calls straight back into GPUI's window procedure, which
//! must not happen while GPUI is in the middle of an update. So every such call goes through
//! [`later`], which runs it from a fresh task once the update is over.

mod shell;

use std::ffi::c_void;
use std::time::Duration;

use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, Global, Pixels, Size,
    Subscription, Task, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
    WindowOptions, point, px, size,
};
use music::Track;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use state::{Sonora, TaskbarSpot};
use tokio::sync::mpsc::{self, UnboundedReceiver};
use ui::ActiveTheme as _;
use views::{PlaylistPicker, PlaylistPickerEvent, TaskbarPlayer, TaskbarPlayerEvent};
use windows::Win32::Foundation::{HWND, RECT};

use shell::{Fit, Signal, Span, Taskbar, Watch, Wish};

/// The widest the player grows, in logical pixels, when the taskbar has the room.
const MOST: f32 = 360.;

/// The narrowest the player still shows, in logical pixels: the cover and the five buttons.
/// With less room than this it stays hidden rather than cover the taskbar's buttons.
const LEAST: f32 = 196.;

/// The clear space between the player and the taskbar's end, its buttons or its notification
/// area, in logical pixels.
const GAP: f32 = 8.;

/// The clear space between the picker's panel and the top of the taskbar, in logical pixels.
const PICKER_GAP: f32 = 4.;

/// How long windows have to stop appearing and going before the taskbar is measured again.
/// Opening an app shows several windows in a burst, and one measurement covers them all.
const SETTLE: Duration = Duration::from_millis(400);

/// When the player is raised a second time after a foreground change. A clicked taskbar can
/// climb back over the player a moment after the click.
const RAISE_AGAIN: Duration = Duration::from_millis(200);

/// Starts following the `taskbar_player` setting. Called once at startup on Windows.
pub fn install(cx: &mut App) {
    let host = cx.new(Host::new);
    cx.set_global(Installed { _host: host });
}

/// Keeps the host alive for the life of the app.
struct Installed {
    _host: Entity<Host>,
}

impl Global for Installed {}

/// Opens and closes the player as the setting changes, and owns both windows while they are open.
struct Host {
    player: Option<Player>,
    picker: Option<Picker>,
    _settings: Subscription,
}

/// The player's window and everything that keeps it on the taskbar.
struct Player {
    window: WindowHandle<TaskbarPlayer>,
    entity: Entity<TaskbarPlayer>,
    hwnd: HWND,
    watch: Watch,
    taskbar: Option<Taskbar>,
    /// The taskbar's buttons as last measured.
    buttons: Option<Vec<Span>>,
    /// Whether the buttons have been measured at least once. Until then the player stays
    /// hidden, since it cannot tell yet whether it would cover them.
    measured: bool,
    fullscreen: bool,
    /// What was last applied to the window, so a layout that changes nothing touches nothing.
    shown: bool,
    placed: Option<RECT>,
    /// Where a drag wants the player's left edge, in physical pixels, while there is one.
    dragged: Option<i32>,
    measuring: Option<Task<()>>,
    settling: Option<Task<()>>,
    raising: Option<Task<()>>,
    _signals: Task<()>,
    _events: Subscription,
}

/// The open playlist picker.
struct Picker {
    window: WindowHandle<PlaylistPicker>,
    hwnd: HWND,
    /// The Add to playlist button's bounds in the player, which the picker opens above.
    anchor: Bounds<Pixels>,
    _events: Subscription,
}

impl Host {
    fn new(cx: &mut Context<Self>) -> Self {
        let settings = Sonora::global(cx).settings.clone();
        let _settings = cx.observe(&settings, |this, _, cx| this.sync(cx));
        let mut this = Self {
            player: None,
            picker: None,
            _settings,
        };
        this.sync(cx);
        this
    }

    /// Opens or closes the player to match the setting.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let wanted = Sonora::global(cx).settings.read(cx).taskbar_player();
        match (wanted, self.player.is_some()) {
            (true, false) => self.open(cx),
            (false, true) => self.close(cx),
            // The place the player was dragged to may have been reset.
            (true, true) => self.apply(cx),
            (false, false) => {}
        }
    }

    fn open(&mut self, cx: &mut Context<Self>) {
        let opened = cx.open_window(popup(false), |window, cx| {
            window.set_rem_size(cx.theme().font_size);
            cx.new(TaskbarPlayer::new)
        });
        let window = match opened {
            Ok(window) => window,
            Err(error) => {
                log::warn!("taskbar player: cannot open the window: {error:#}");
                return;
            }
        };
        state::accessory(AnyWindowHandle::from(window).window_id(), cx);
        let Ok((entity, Some(hwnd))) =
            window.update(cx, |_, window, cx| (cx.entity(), handle(window)))
        else {
            log::warn!("taskbar player: the window has no handle");
            window
                .update(cx, |_, window, _| window.remove_window())
                .ok();
            return;
        };
        // GPUI showed it at a pixel's size; it stays hidden until the taskbar is measured.
        later(cx, move || {
            shell::show(hwnd, false);
            shell::dress(hwnd, false);
        });

        let taskbar = Taskbar::find();
        let (sender, receiver) = mpsc::unbounded_channel();
        let watch = Watch::new(hwnd, taskbar.map(|taskbar| taskbar.hwnd), sender);
        let _events = cx.subscribe(
            &entity,
            |this, _, event: &TaskbarPlayerEvent, cx| match event {
                TaskbarPlayerEvent::Pick { track, anchor } => {
                    this.toggle_picker((**track).clone(), *anchor, cx)
                }
                TaskbarPlayerEvent::Drag { grab } => this.drag(*grab, cx),
                TaskbarPlayerEvent::Drop => this.settle_drag(cx),
            },
        );
        log::info!(
            "taskbar player: opened, taskbar at {:?}",
            taskbar.map(|taskbar| taskbar.rect)
        );

        self.player = Some(Player {
            window,
            entity,
            hwnd,
            watch,
            taskbar,
            buttons: None,
            measured: false,
            fullscreen: shell::fullscreen_now(),
            shown: false,
            placed: None,
            dragged: None,
            measuring: None,
            settling: None,
            raising: None,
            _signals: listen(receiver, cx),
            _events,
        });
        self.measure(cx);
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.close_picker(cx);
        let Some(player) = self.player.take() else {
            return;
        };
        let window = player.window;
        // The hooks and the subclass come off while the window still exists.
        drop(player);
        cx.defer(move |cx| {
            window
                .update(cx, |_, window, _| window.remove_window())
                .ok();
        });
        log::info!("taskbar player: closed");
    }

    fn signal(&mut self, signal: Signal, cx: &mut Context<Self>) {
        match signal {
            Signal::Raise => {
                self.raise(cx);
                self.raise_again(cx);
            }
            Signal::Relayout => self.relayout(cx),
            Signal::Remeasure => self.settle(cx),
            Signal::Fullscreen(fullscreen) => {
                if let Some(player) = self.player.as_mut() {
                    player.fullscreen = fullscreen;
                }
                self.apply(cx);
            }
            Signal::Restarted => {
                let Some(player) = self.player.as_mut() else {
                    return;
                };
                log::info!("taskbar player: explorer restarted, finding the taskbar again");
                player.taskbar = Taskbar::find();
                player
                    .watch
                    .retarget(player.taskbar.map(|taskbar| taskbar.hwnd));
                self.measure(cx);
            }
        }
    }

    /// Reads the taskbar again, measuring its buttons afresh when it moved or changed size.
    fn relayout(&mut self, cx: &mut Context<Self>) {
        let Some(player) = self.player.as_mut() else {
            return;
        };
        let taskbar = Taskbar::find();
        let moved = taskbar.map(|taskbar| (taskbar.rect, taskbar.tray_left))
            != player
                .taskbar
                .map(|taskbar| (taskbar.rect, taskbar.tray_left));
        player.taskbar = taskbar;
        // An auto-hidden taskbar only slides; its buttons stay where they were.
        let slid = taskbar.is_some_and(|taskbar| taskbar.autohides());
        match moved && !slid {
            true => self.measure(cx),
            false => self.apply(cx),
        }
    }

    /// Measures the taskbar's buttons once windows stop coming and going.
    fn settle(&mut self, cx: &mut Context<Self>) {
        let Some(player) = self.player.as_mut() else {
            return;
        };
        player.settling = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SETTLE).await;
            this.update(cx, |this, cx| this.measure(cx)).ok();
        }));
    }

    /// Reads where the taskbar's buttons are, on a background thread, then lays the player out
    /// again. A measurement still running is dropped for the new one.
    fn measure(&mut self, cx: &mut Context<Self>) {
        let Some(player) = self.player.as_mut() else {
            return;
        };
        let Some(taskbar) = player.taskbar else {
            player.measured = true;
            self.apply(cx);
            return;
        };
        let (hwnd, tray_left) = (taskbar.hwnd.0 as isize, taskbar.tray_left);
        let job = cx
            .background_executor()
            .spawn(async move { shell::buttons(hwnd, tray_left) });
        player.measuring = Some(cx.spawn(async move |this, cx| {
            let buttons = job.await;
            this.update(cx, |this, cx| {
                if let Some(player) = this.player.as_mut() {
                    if buttons.is_some() {
                        player.buttons = buttons;
                    }
                    player.measured = true;
                    player.measuring = None;
                }
                this.apply(cx);
            })
            .ok();
        }));
    }

    /// Puts the player as near as the taskbar has room to where it was dragged, or hides it when
    /// there is none, a full screen app is up or the taskbar has slid away.
    fn apply(&mut self, cx: &mut Context<Self>) {
        let spot = Sonora::global(cx).settings.read(cx).taskbar_player_spot();
        let Some(player) = self.player.as_mut() else {
            return;
        };
        let rect = player
            .taskbar
            .filter(|taskbar| taskbar.shown() && player.measured)
            .and_then(|taskbar| {
                let scale = |logical: f32| (logical * taskbar.scale).round() as i32;
                let wish = match (player.dragged, spot) {
                    (Some(left), _) => Wish::Left(left),
                    (None, Some(TaskbarSpot::Left(offset))) => {
                        Wish::Left(taskbar.rect.left + scale(offset))
                    }
                    (None, Some(TaskbarSpot::Right(offset))) => {
                        Wish::Right(taskbar.tray_left - scale(offset))
                    }
                    (None, None) => Wish::Usual,
                };
                let fit = Fit {
                    most: scale(MOST),
                    least: scale(LEAST),
                    gap: scale(GAP),
                };
                shell::slot(&taskbar, player.buttons.as_deref(), wish, fit)
            });
        let shown = rect.is_some() && !player.fullscreen;
        let target = rect.filter(|_| shown);
        if shown == player.shown && (target.is_none() || target == player.placed) {
            return;
        }
        player.shown = shown;
        if target.is_some() {
            player.placed = target;
        }
        let hwnd = player.hwnd;
        later(cx, move || {
            if let Some(rect) = target {
                shell::place(hwnd, rect);
            }
            shell::show(hwnd, shown);
        });
        if !shown {
            self.close_picker(cx);
        }
    }

    /// Moves the player along under the pointer, which holds it `grab` from its left edge. It
    /// goes only where there is room, so where it is seen is where it will stay.
    fn drag(&mut self, grab: Pixels, cx: &mut Context<Self>) {
        let Some(player) = self.player.as_mut() else {
            return;
        };
        let Some(cursor) = shell::cursor() else {
            return;
        };
        let grab = f32::from(grab) * shell::scale(player.hwnd);
        player.dragged = Some(cursor.x - grab.round() as i32);
        self.close_picker(cx);
        self.apply(cx);
    }

    /// Ends a drag and remembers where the player is, from whichever end it is nearer to.
    fn settle_drag(&mut self, cx: &mut Context<Self>) {
        let Some(player) = self.player.as_mut() else {
            return;
        };
        if player.dragged.take().is_none() {
            return;
        }
        let (Some(taskbar), Some(rect)) = (player.taskbar, player.placed) else {
            return;
        };
        let logical = |physical: i32| physical as f32 / taskbar.scale;
        let left = rect.left - taskbar.rect.left;
        let right = taskbar.tray_left - rect.right;
        let spot = match left <= right {
            true => TaskbarSpot::Left(logical(left)),
            false => TaskbarSpot::Right(logical(right)),
        };
        let settings = Sonora::global(cx).settings.clone();
        settings.update(cx, |settings, cx| {
            settings.set_taskbar_player_spot(Some(spot), cx)
        });
    }

    /// Brings the player, and the picker above it, back over the taskbar.
    fn raise(&self, cx: &mut Context<Self>) {
        let Some(player) = self.player.as_ref().filter(|player| player.shown) else {
            return;
        };
        let (hwnd, picker) = (player.hwnd, self.picker.as_ref().map(|picker| picker.hwnd));
        later(cx, move || {
            shell::raise(hwnd);
            if let Some(picker) = picker {
                shell::raise(picker);
            }
        });
    }

    fn raise_again(&mut self, cx: &mut Context<Self>) {
        let Some(player) = self.player.as_mut() else {
            return;
        };
        player.raising = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RAISE_AGAIN).await;
            this.update(cx, |this, cx| this.raise(cx)).ok();
        }));
    }

    /// Opens the picker above the Add to playlist button, or closes it when it is open.
    fn toggle_picker(&mut self, track: Track, anchor: Bounds<Pixels>, cx: &mut Context<Self>) {
        if self.picker.is_some() {
            self.close_picker(cx);
            return;
        }
        let Some(player) = self.player.as_ref() else {
            return;
        };
        let opened = cx.open_window(popup(true), |window, cx| {
            window.set_rem_size(cx.theme().font_size);
            cx.new(|cx| PlaylistPicker::new(track, window, cx))
        });
        let window = match opened {
            Ok(window) => window,
            Err(error) => {
                log::warn!("taskbar player: cannot open the playlist picker: {error:#}");
                return;
            }
        };
        state::accessory(AnyWindowHandle::from(window).window_id(), cx);
        let Ok((entity, Some(hwnd))) =
            window.update(cx, |_, window, cx| (cx.entity(), handle(window)))
        else {
            window
                .update(cx, |_, window, _| window.remove_window())
                .ok();
            return;
        };
        later(cx, move || shell::dress(hwnd, true));
        let _events = cx.subscribe(
            &entity,
            |this, _, event: &PlaylistPickerEvent, cx| match event {
                PlaylistPickerEvent::Fit { size, margin } => this.fit_picker(*size, *margin, cx),
                PlaylistPickerEvent::Close => this.close_picker(cx),
            },
        );
        player
            .entity
            .update(cx, |player, cx| player.set_picking(true, cx));
        self.picker = Some(Picker {
            window,
            hwnd,
            anchor,
            _events,
        });
    }

    /// Sizes the picker to its panel and stands it on the taskbar's top edge, its right side
    /// level with the Add to playlist button's and kept on the taskbar's monitor.
    fn fit_picker(&mut self, size: Size<Pixels>, margin: Pixels, cx: &mut Context<Self>) {
        let (Some(player), Some(picker)) = (self.player.as_ref(), self.picker.as_ref()) else {
            return;
        };
        let Some(widget) = shell::window_rect(player.hwnd) else {
            return;
        };
        let scale = shell::scale(player.hwnd);
        let physical = |logical: Pixels| (f32::from(logical) * scale).round() as i32;
        let (width, height, margin) = (
            physical(size.width),
            physical(size.height),
            physical(margin),
        );
        let floor = player
            .taskbar
            .map_or(widget.left, |taskbar| taskbar.monitor.left);
        let right = widget.left + physical(picker.anchor.right()) + margin;
        let gap = (PICKER_GAP * scale).round() as i32;
        let left = (right - width).max(floor + gap - margin);
        let bottom = widget.top - gap + margin;
        let rect = RECT {
            left,
            top: bottom - height,
            right: left + width,
            bottom,
        };
        let hwnd = picker.hwnd;
        later(cx, move || shell::place(hwnd, rect));
    }

    fn close_picker(&mut self, cx: &mut Context<Self>) {
        let Some(picker) = self.picker.take() else {
            return;
        };
        if let Some(player) = self.player.as_ref() {
            player
                .entity
                .update(cx, |player, cx| player.set_picking(false, cx));
        }
        let window = picker.window;
        cx.defer(move |cx| {
            window
                .update(cx, |_, window, _| window.remove_window())
                .ok();
        });
    }
}

/// Feeds what the shell reports to the host, one signal at a time.
fn listen(mut receiver: UnboundedReceiver<Signal>, cx: &mut Context<Host>) -> Task<()> {
    cx.spawn(async move |this, cx| {
        while let Some(signal) = receiver.recv().await {
            if this.update(cx, |this, cx| this.signal(signal, cx)).is_err() {
                break;
            }
        }
    })
}

/// The window both the player and the picker open as: a borderless popup with a see-through
/// background, a pixel in size until it is placed. Only the picker takes focus.
fn popup(focus: bool) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(1.), px(1.)),
        })),
        titlebar: None,
        focus,
        show: true,
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Transparent,
        ..Default::default()
    }
}

/// Runs `act` from a fresh task, once GPUI has finished the update it is in. Window moves call
/// back into GPUI's window procedure, which must not happen mid-update.
fn later(cx: &mut Context<Host>, act: impl FnOnce() + 'static) {
    cx.spawn(async move |_, _| act()).detach();
}

fn handle(window: &Window) -> Option<HWND> {
    let RawWindowHandle::Win32(handle) = HasWindowHandle::window_handle(window).ok()?.as_raw()
    else {
        return None;
    };
    Some(HWND(handle.hwnd.get() as *mut c_void))
}
