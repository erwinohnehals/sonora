//! The Win32 half of the taskbar player: where the taskbar has room for it, and the shell events
//! that move, hide or bury it.
//!
//! Windows 11 has no way left to put a control inside the taskbar, so the player is an ordinary
//! topmost tool window laid over a stretch of the taskbar that has no buttons. That brings four
//! duties, all handled here:
//!
//! - **Finding room.** The taskbar draws its buttons in XAML, so their extent is read through UI
//!   Automation, which is slow enough to run off the main thread.
//! - **Staying on top.** The taskbar is topmost too, and climbs over the player whenever it is
//!   activated, so every foreground change raises the player again.
//! - **Getting out of the way.** Registered as an app bar, the player hears when a full screen
//!   app opens, and hides for it the way the taskbar does.
//! - **Following the taskbar.** Its moves, a display change and Explorer restarting all lead to a
//!   fresh layout.
//!
//! Every one of those reaches the GPUI side as a [`Signal`] on a channel, never as a direct call,
//! because a window procedure or event hook may run while GPUI is mid-update.

use std::cell::RefCell;
use std::ffi::c_void;

use tokio::sync::mpsc::UnboundedSender;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
    DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, HWINEVENTHOOK, IUIAutomation, SetWinEventHook, TreeScope_Descendants,
    UIA_ButtonControlTypeId, UIA_ControlTypePropertyId, UnhookWinEvent,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Shell::{
    ABE_BOTTOM, ABM_GETSTATE, ABM_NEW, ABM_REMOVE, ABN_FULLSCREENAPP, ABN_POSCHANGED,
    ABN_STATECHANGE, ABS_AUTOHIDE, APPBARDATA, DefSubclassProc, QUNS_BUSY, QUNS_PRESENTATION_MODE,
    QUNS_RUNNING_D3D_FULL_SCREEN, RemoveWindowSubclass, SHAppBarMessage,
    SHQueryUserNotificationState, SetWindowSubclass,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND,
    FindWindowExW, FindWindowW, GA_ROOT, GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GetAncestor,
    GetCursorPos, GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId,
    HWND_TOPMOST, OBJID_WINDOW, RegisterWindowMessageW, SW_HIDE, SW_SHOWNOACTIVATE,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
    WM_APP, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_SETTINGCHANGE, WS_CAPTION, WS_EX_APPWINDOW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::w;

/// The message the shell sends app bar notifications with. Any id in the app range will do, since
/// only the player's own window receives it.
const APPBAR_CALLBACK: u32 = WM_APP + 0x51;

/// Our slot in the player window's subclass chain. Unlike the thumbnail toolbar's, which sits on
/// the main window.
const SUBCLASS: usize = 0x736f_6e70;

/// The id `SetWinEventHook` reports for the window itself rather than a part of it.
const CHILDID_SELF: i32 = 0;

/// What the shell did that the GPUI side has to act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// Something else came to the front, which may have buried the player under the taskbar.
    Raise,
    /// The taskbar moved, resized or slid away, or the display changed under it.
    Relayout,
    /// A window that has a taskbar button appeared or went away, so the buttons may now reach
    /// further.
    Remeasure,
    /// A full screen app opened (`true`) or closed (`false`) on the taskbar's monitor.
    Fullscreen(bool),
    /// Explorer restarted, so the taskbar and the app bar registration are new.
    Restarted,
}

/// The taskbar as it stands, in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Taskbar {
    pub hwnd: HWND,
    pub rect: RECT,
    /// The left edge of the notification area, which the player stays left of.
    pub tray_left: i32,
    /// The taskbar's monitor, for telling whether an auto-hidden taskbar has slid out of view.
    pub monitor: RECT,
    /// Physical pixels per logical one on the taskbar's monitor.
    pub scale: f32,
}

impl Taskbar {
    /// Reads the primary taskbar. `None` when there is none, or when it stands on a side of the
    /// screen, where a player one row tall would not fit.
    pub fn find() -> Option<Self> {
        let hwnd = unsafe { FindWindowW(w!("Shell_TrayWnd"), None) }.ok()?;
        let rect = window_rect(hwnd)?;
        if rect.right - rect.left <= rect.bottom - rect.top {
            return None;
        }
        let tray = unsafe { FindWindowExW(Some(hwnd), None, w!("TrayNotifyWnd"), None) }.ok();
        let tray_left = tray
            .and_then(window_rect)
            .map(|tray| tray.left)
            .filter(|left| *left > rect.left)
            .unwrap_or(rect.right);
        let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY) };
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let monitor = match unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            true => info.rcMonitor,
            false => rect,
        };
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        Some(Self {
            hwnd,
            rect,
            tray_left,
            monitor,
            scale: match dpi {
                0 => 1.,
                dpi => dpi as f32 / 96.,
            },
        })
    }

    /// Whether the taskbar is in view. An auto-hidden one slides almost entirely off its monitor
    /// while the pointer is away, and the player goes with it.
    pub fn shown(&self) -> bool {
        let visible =
            self.rect.bottom.min(self.monitor.bottom) - self.rect.top.max(self.monitor.top);
        visible >= (self.rect.bottom - self.rect.top) / 2
    }

    /// Whether the taskbar hides itself until the pointer reaches the screen's edge.
    pub fn autohides(&self) -> bool {
        let mut data = APPBARDATA {
            cbSize: size_of::<APPBARDATA>() as u32,
            hWnd: self.hwnd,
            uEdge: ABE_BOTTOM,
            ..Default::default()
        };
        let state = unsafe { SHAppBarMessage(ABM_GETSTATE, &mut data) } as u32;
        state & ABS_AUTOHIDE != 0
    }
}

/// A stretch of the taskbar from its left to its right edge, in physical pixels.
pub type Span = (i32, i32);

/// Where on the taskbar the player would like to be, in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wish {
    /// At the left end of the taskbar, or against the notification area when the taskbar's
    /// buttons already have the left end.
    Usual,
    /// With its left edge here.
    Left(i32),
    /// With its right edge here.
    Right(i32),
}

/// How wide the player may be and how far it keeps from what it sits beside, in physical pixels.
#[derive(Clone, Copy, Debug)]
pub struct Fit {
    pub most: i32,
    pub least: i32,
    pub gap: i32,
}

/// Where the player goes: the free place nearest to `wish`, as tall as the taskbar and as wide as
/// the room there lets it be. `None` when no free stretch is wide enough. `buttons` are the
/// taskbar's buttons from left to right, or `None` when they could not be read.
pub fn slot(taskbar: &Taskbar, buttons: Option<&[Span]>, wish: Wish, fit: Fit) -> Option<RECT> {
    let free = free(taskbar, buttons.unwrap_or_default(), fit);
    let width = |(left, right): Span| (right - left).min(fit.most);
    let (span, left) = match wish {
        Wish::Usual => {
            let (first, last) = (*free.first()?, *free.last()?);
            // Buttons that start at the left end leave one stretch, whose left edge moves with
            // every app opened, so there the player holds on to the other end. So it does
            // unmeasured, since the left end is where a button is likeliest.
            let clear = free.len() > 1 || first.0 <= taskbar.rect.left + fit.gap;
            match clear && buttons.is_some() {
                true => (first, first.0),
                false => (last, last.1 - width(last)),
            }
        }
        Wish::Left(wanted) => free
            .into_iter()
            .map(|span| (span, wanted.clamp(span.0, span.1 - width(span))))
            .min_by_key(|(_, left)| (left - wanted).abs())?,
        Wish::Right(wanted) => free
            .into_iter()
            .map(|span| {
                let width = width(span);
                (span, (wanted - width).clamp(span.0, span.1 - width))
            })
            .min_by_key(|(span, left)| (left + width(*span) - wanted).abs())?,
    };
    Some(RECT {
        left,
        top: taskbar.rect.top,
        right: left + width(span),
        bottom: taskbar.rect.bottom,
    })
}

/// The stretches left of the notification area that are clear of every button and wide enough for
/// the player, from left to right.
fn free(taskbar: &Taskbar, buttons: &[Span], fit: Fit) -> Vec<Span> {
    let mut free = Vec::new();
    let mut reached = taskbar.rect.left;
    let end = (taskbar.tray_left, taskbar.tray_left);
    for (left, right) in buttons.iter().copied().chain([end]) {
        let span = (reached + fit.gap, left - fit.gap);
        if span.1 - span.0 >= fit.least {
            free.push(span);
        }
        reached = reached.max(right);
    }
    free
}

/// The taskbar buttons left of the notification area, from left to right, read through UI
/// Automation. Blocks for tens of milliseconds, longer if Explorer is busy, so it runs on a
/// background thread. `None` when UI Automation cannot answer, and the caller then keeps what it
/// measured last.
pub fn buttons(taskbar: isize, tray_left: i32) -> Option<Vec<Span>> {
    let taskbar = HWND(taskbar as *mut c_void);
    let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let buttons = measure(taskbar, tray_left);
    if initialized {
        unsafe { CoUninitialize() };
    }
    match buttons {
        Ok(buttons) => Some(buttons),
        Err(error) => {
            log::debug!("taskbar player: cannot measure the taskbar buttons: {error:#}");
            None
        }
    }
}

fn measure(taskbar: HWND, tray_left: i32) -> windows::core::Result<Vec<Span>> {
    let mut spans = Vec::new();
    unsafe {
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
        let root = automation.ElementFromHandle(taskbar)?;
        let buttons = VARIANT::from(UIA_ButtonControlTypeId.0);
        let condition = automation.CreatePropertyCondition(UIA_ControlTypePropertyId, &buttons)?;
        let found = root.FindAll(TreeScope_Descendants, &condition)?;
        for at in 0..found.Length()? {
            let Ok(element) = found.GetElement(at) else {
                continue;
            };
            let Ok(rect) = element.CurrentBoundingRectangle() else {
                continue;
            };
            if rect.right > rect.left && rect.right <= tray_left {
                spans.push((rect.left, rect.right));
            }
        }
    }
    spans.sort_unstable();
    Ok(spans)
}

/// Whether a full screen app or presentation is up right now, for the player's first showing.
/// Later changes arrive as [`Signal::Fullscreen`].
pub fn fullscreen_now() -> bool {
    let Ok(state) = (unsafe { SHQueryUserNotificationState() }) else {
        return false;
    };
    state == QUNS_BUSY || state == QUNS_RUNNING_D3D_FULL_SCREEN || state == QUNS_PRESENTATION_MODE
}

/// Turns a GPUI popup into something that lives on the taskbar: it has no frame, so it draws all
/// the way to its edges, a click on it never takes focus from the app in front unless it
/// `activates`, and DWM draws no rounded corners or border around its transparent edges.
pub fn dress(hwnd: HWND, activates: bool) {
    unsafe {
        // GPUI's popups are overlapped windows, whose caption makes Windows keep an invisible
        // frame at the sides and the bottom.
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        let frameless = (style & !WS_CAPTION.0) | WS_POPUP.0;
        SetWindowLongPtrW(hwnd, GWL_STYLE, frameless as isize);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        if !activates {
            let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | WS_EX_NOACTIVATE.0 as isize);
        }
        let corner = DWMWCP_DONOTROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as *const c_void,
            size_of_val(&corner) as u32,
        );
        let border = DWMWA_COLOR_NONE;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &border as *const _ as *const c_void,
            size_of_val(&border) as u32,
        );
    }
}

/// Moves `hwnd` to `rect` above every other topmost window, without activating it.
pub fn place(hwnd: HWND, rect: RECT) {
    let placed = unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        )
    };
    if let Err(error) = placed {
        log::debug!("taskbar player: cannot place the window: {error:#}");
    }
}

/// Brings `hwnd` back above the taskbar without moving or activating it.
pub fn raise(hwnd: HWND) {
    let _ = unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        )
    };
}

/// Shows or hides `hwnd` without activating it.
pub fn show(hwnd: HWND, shown: bool) {
    let command = match shown {
        true => SW_SHOWNOACTIVATE,
        false => SW_HIDE,
    };
    let _ = unsafe { ShowWindow(hwnd, command) };
}

/// Where the pointer is on the screen, in physical pixels.
pub fn cursor() -> Option<POINT> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) }.ok()?;
    Some(point)
}

pub fn window_rect(hwnd: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    Some(rect)
}

/// Physical pixels per logical one for `hwnd`.
pub fn scale(hwnd: HWND) -> f32 {
    match unsafe { GetDpiForWindow(hwnd) } {
        0 => 1.,
        dpi => dpi as f32 / 96.,
    }
}

/// Where the hooks send what they see. Event hooks carry no context pointer, so the one sink
/// lives in a thread local; the hooks run on the thread that set them, which is GPUI's.
struct Sink {
    sender: UnboundedSender<Signal>,
    taskbar: HWND,
}

thread_local! {
    static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
}

/// The hooks, the app bar registration and the subclass on the player's window. Dropping it takes
/// all of them down.
pub struct Watch {
    hwnd: HWND,
    hooks: Vec<HWINEVENTHOOK>,
    appbar: bool,
    subclassed: bool,
    /// Boxed so the subclass procedure can hold a stable pointer to it, and kept only for that.
    _state: Box<WatchState>,
}

/// What the player window's subclass procedure needs.
struct WatchState {
    sender: UnboundedSender<Signal>,
    restarted: u32,
}

impl Watch {
    /// Starts watching the shell on behalf of the player window `hwnd`.
    pub fn new(hwnd: HWND, taskbar: Option<HWND>, sender: UnboundedSender<Signal>) -> Self {
        let restarted = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
        let state = Box::new(WatchState {
            sender: sender.clone(),
            restarted,
        });
        SINK.with(|sink| {
            *sink.borrow_mut() = Some(Sink {
                sender,
                taskbar: taskbar.unwrap_or_default(),
            });
        });

        let refdata = &*state as *const WatchState as usize;
        let subclassed =
            unsafe { SetWindowSubclass(hwnd, Some(subclass), SUBCLASS, refdata) }.as_bool();
        if !subclassed {
            log::warn!(
                "taskbar player: cannot listen to the shell; it will not hide for full screen apps"
            );
        }

        let mut watch = Self {
            hwnd,
            hooks: Vec::new(),
            appbar: false,
            subclassed,
            _state: state,
        };
        watch.register();
        watch.hook(taskbar);
        watch
    }

    /// Points the hooks at a new taskbar, after Explorer restarted.
    pub fn retarget(&mut self, taskbar: Option<HWND>) {
        SINK.with(|sink| {
            if let Some(sink) = sink.borrow_mut().as_mut() {
                sink.taskbar = taskbar.unwrap_or_default();
            }
        });
        self.unhook();
        self.hook(taskbar);
        self.unregister();
        self.register();
    }

    /// Registers as an app bar without claiming any of the screen, which is what makes the shell
    /// send full screen and taskbar position notices.
    fn register(&mut self) {
        let mut data = APPBARDATA {
            cbSize: size_of::<APPBARDATA>() as u32,
            hWnd: self.hwnd,
            uCallbackMessage: APPBAR_CALLBACK,
            ..Default::default()
        };
        self.appbar = unsafe { SHAppBarMessage(ABM_NEW, &mut data) } != 0;
        if !self.appbar {
            log::debug!("taskbar player: the shell refused the app bar registration");
        }
    }

    fn unregister(&mut self) {
        if !std::mem::take(&mut self.appbar) {
            return;
        }
        let mut data = APPBARDATA {
            cbSize: size_of::<APPBARDATA>() as u32,
            hWnd: self.hwnd,
            ..Default::default()
        };
        unsafe { SHAppBarMessage(ABM_REMOVE, &mut data) };
    }

    fn hook(&mut self, taskbar: Option<HWND>) {
        let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
        let mut set = |event: u32, process: u32| {
            let hook =
                unsafe { SetWinEventHook(event, event, None, Some(on_event), process, 0, flags) };
            if hook.is_invalid() {
                log::debug!("taskbar player: cannot hook shell event {event:#x}");
            } else {
                self.hooks.push(hook);
            }
        };
        set(EVENT_SYSTEM_FOREGROUND, 0);
        set(EVENT_OBJECT_SHOW, 0);
        set(EVENT_OBJECT_HIDE, 0);
        // Location changes fire for every caret and window anywhere, so only Explorer's are asked
        // for, and of those only the taskbar's own count.
        if let Some(taskbar) = taskbar {
            let mut explorer = 0u32;
            unsafe { GetWindowThreadProcessId(taskbar, Some(&mut explorer)) };
            if explorer != 0 {
                set(EVENT_OBJECT_LOCATIONCHANGE, explorer);
            }
        }
    }

    fn unhook(&mut self) {
        for hook in self.hooks.drain(..) {
            let _ = unsafe { UnhookWinEvent(hook) };
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.unhook();
        self.unregister();
        if self.subclassed {
            let _ = unsafe { RemoveWindowSubclass(self.hwnd, Some(subclass), SUBCLASS) };
        }
        SINK.with(|sink| sink.borrow_mut().take());
    }
}

/// The event hook. Sorts what Windows reports into the few [`Signal`]s the player cares about and
/// drops the rest, which is nearly everything.
unsafe extern "system" fn on_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    object: i32,
    child: i32,
    _thread: u32,
    _time: u32,
) {
    if object != OBJID_WINDOW.0 || child != CHILDID_SELF {
        return;
    }
    SINK.with(|sink| {
        let sink = sink.borrow();
        let Some(sink) = sink.as_ref() else {
            return;
        };
        let signal = match event {
            EVENT_SYSTEM_FOREGROUND => Some(Signal::Raise),
            EVENT_OBJECT_LOCATIONCHANGE if hwnd == sink.taskbar => Some(Signal::Relayout),
            EVENT_OBJECT_SHOW | EVENT_OBJECT_HIDE if has_taskbar_button(hwnd) => {
                Some(Signal::Remeasure)
            }
            _ => None,
        };
        if let Some(signal) = signal {
            sink.sender.send(signal).ok();
        }
    });
}

/// Whether `hwnd` is the kind of window the taskbar gives a button: a top-level window that is
/// either forced onto the taskbar or neither owned nor a tool window.
fn has_taskbar_button(hwnd: HWND) -> bool {
    unsafe {
        if GetAncestor(hwnd, GA_ROOT) != hwnd {
            return false;
        }
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if style & WS_EX_APPWINDOW.0 != 0 {
            return true;
        }
        let owned = GetWindow(hwnd, GW_OWNER).is_ok_and(|owner| !owner.is_invalid());
        !owned && style & WS_EX_TOOLWINDOW.0 == 0
    }
}

/// The player window's procedure, ahead of GPUI's: app bar notices, display changes and Explorer
/// restarts become [`Signal`]s, and everything goes on down the chain.
unsafe extern "system" fn subclass(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    refdata: usize,
) -> LRESULT {
    let state = unsafe { &*(refdata as *const WatchState) };
    let signal = match message {
        APPBAR_CALLBACK => match wparam.0 as u32 {
            ABN_FULLSCREENAPP => Some(Signal::Fullscreen(lparam.0 != 0)),
            ABN_POSCHANGED | ABN_STATECHANGE => Some(Signal::Relayout),
            _ => None,
        },
        WM_DISPLAYCHANGE | WM_DPICHANGED | WM_SETTINGCHANGE => Some(Signal::Relayout),
        message if message == state.restarted => Some(Signal::Restarted),
        _ => None,
    };
    if let Some(signal) = signal {
        state.sender.send(signal).ok();
    }
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}
