//! Which of the app's windows is the main one. The taskbar player and its playlist picker are
//! windows too, so anything that means "the window" (the tray's show and hide, quitting when the
//! last one closes) asks [`main_windows`] instead of `cx.windows()`. Showing the main window is
//! platform code only `sonora` links against, so it installs that once at startup and `views`
//! reaches it through [`show_main_window`].

use std::collections::HashSet;
use std::rc::Rc;

use gpui::{AnyWindowHandle, App, Global, WindowId};

type Show = Rc<dyn Fn(&mut App)>;

/// The windows that are not the main window.
#[derive(Default)]
struct Accessories(HashSet<WindowId>);

impl Global for Accessories {}

struct ShowHook(Show);

impl Global for ShowHook {}

/// Marks `window` as an accessory, so it never counts as the main window. Its id is forgotten
/// when it closes.
pub fn accessory(window: WindowId, cx: &mut App) {
    let fresh = !cx.has_global::<Accessories>();
    cx.default_global::<Accessories>().0.insert(window);
    if fresh {
        cx.on_window_closed(|cx, closed| {
            cx.default_global::<Accessories>().0.remove(&closed);
        })
        .detach();
    }
}

/// Whether `window` is an accessory rather than the main window.
pub fn is_accessory(window: WindowId, cx: &App) -> bool {
    cx.try_global::<Accessories>()
        .is_some_and(|accessories| accessories.0.contains(&window))
}

/// The open windows that are not accessories. Empty while the main window is closed to the tray,
/// even with the taskbar player still showing.
pub fn main_windows(cx: &App) -> Vec<AnyWindowHandle> {
    cx.windows()
        .into_iter()
        .filter(|window| !is_accessory(window.window_id(), cx))
        .collect()
}

/// Registers how the main window is brought back: raised when it is open, opened again when it
/// was closed to the tray.
pub fn install_show_main_window(show: impl Fn(&mut App) + 'static, cx: &mut App) {
    cx.set_global(ShowHook(Rc::new(show)));
}

/// Raises the main window, opening it again if it was closed. Does nothing until `sonora` has
/// installed the hook.
pub fn show_main_window(cx: &mut App) {
    let Some(show) = cx.try_global::<ShowHook>().map(|hook| hook.0.clone()) else {
        return;
    };
    show(cx);
}
