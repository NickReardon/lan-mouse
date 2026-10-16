use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ptr::addr_of_mut;

use std::default::Default;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;
use windows::Win32::Foundation::{FALSE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICEW, ENUM_CURRENT_SETTINGS,
    EnumDisplayDevicesW, EnumDisplaySettingsW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY,
};
use windows::core::{PCWSTR, w};

use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DispatchMessageW, EDD_GET_DEVICE_INTERFACE_NAME, GetMessageW,
    HOOKPROC, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG, MSLLHOOKSTRUCT, PostThreadMessageW,
    RegisterClassW, SetWindowsHookExW, TranslateMessage, WH_KEYBOARD_LL, WH_MOUSE_LL, WINDOW_STYLE,
    WM_DISPLAYCHANGE, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
    WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WM_USER, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW, WNDPROC,
};

use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
    scancode::{self, Linux},
};

use super::{CaptureEvent, Position, display_util};

pub(crate) struct EventThread {
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
    thread: Option<thread::JoinHandle<()>>,
    thread_id: u32,
}

impl EventThread {
    pub(crate) fn new(event_tx: Sender<(Position, CaptureEvent)>) -> Self {
        let request_buffer = Default::default();
        let (thread, thread_id) = start(event_tx, Arc::clone(&request_buffer));
        Self {
            request_buffer,
            thread: Some(thread),
            thread_id,
        }
    }

    pub(crate) fn release_capture(&self) {
        self.signal(RequestType::Release);
    }

    pub(crate) fn release_at(
        &self,
        pos: Position,
        fraction: u16,
    ) -> tokio::sync::oneshot::Receiver<Result<(), super::CaptureError>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.client_update(ClientUpdate::ReleaseAt(pos, fraction, tx));
        rx
    }

    pub(crate) fn create(&self, pos: Position) {
        self.client_update(ClientUpdate::Create(pos));
    }

    pub(crate) fn destroy(&self, pos: Position) {
        self.client_update(ClientUpdate::Destroy(pos));
    }

    fn exit(&self) {
        self.signal(RequestType::Exit);
    }

    fn client_update(&self, request: ClientUpdate) {
        {
            let mut requests = self.request_buffer.lock().unwrap();
            requests.push(request);
        }
        self.signal(RequestType::ClientUpdate);
    }

    fn signal(&self, event_type: RequestType) {
        let id = self.thread_id;
        unsafe { PostThreadMessageW(id, WM_USER, WPARAM(event_type as usize), LPARAM(0)).unwrap() };
    }
}

impl Drop for EventThread {
    fn drop(&mut self) {
        self.exit();
        let _ = self.thread.take().expect("thread").join();
    }
}

enum RequestType {
    ClientUpdate = 0,
    Release = 1,
    Exit = 2,
}

enum ClientUpdate {
    ReleaseAt(
        Position,
        u16,
        tokio::sync::oneshot::Sender<Result<(), super::CaptureError>>,
    ),
    Create(Position),
    Destroy(Position),
}

fn blocking_send_event(pos: Position, event: CaptureEvent) {
    EVENT_TX.with_borrow_mut(|tx| tx.as_mut().unwrap().blocking_send((pos, event)).unwrap())
}

fn try_send_event(
    pos: Position,
    event: CaptureEvent,
) -> Result<(), TrySendError<(Position, CaptureEvent)>> {
    EVENT_TX.with_borrow_mut(|tx| tx.as_mut().unwrap().try_send((pos, event)))
}

thread_local! {
    /// all configured clients
    static CLIENTS: RefCell<HashSet<Position>> = RefCell::new(HashSet::new());
    /// currently active client
    static ACTIVE_CLIENT: Cell<Option<Position>> = const { Cell::new(None) };
    /// input event channel
    static EVENT_TX: RefCell<Option<Sender<(Position, CaptureEvent)>>> = const { RefCell::new(None) };
    /// position of barrier entry
    static ENTRY_POINT: Cell<(i32, i32)> = const { Cell::new((0, 0)) };
    /// previous mouse position
    static PREV_POS: Cell<Option<(i32, i32)>> = const { Cell::new(None) };
    /// virtual-key codes of keys that went down while Windows had control
    static LOCAL_KEYS: RefCell<HashSet<u32>> = RefCell::new(HashSet::new());
    /// displays and generation counter
    static DISPLAYS: RefCell<(Vec<RECT>, i32)> = const { RefCell::new((Vec::new(), 0)) };
}

fn get_msg() -> Option<MSG> {
    unsafe {
        let mut msg = std::mem::zeroed();
        let ret = GetMessageW(addr_of_mut!(msg), None, 0, 0);
        match ret.0 {
            0 => None,
            x if x > 0 => Some(msg),
            _ => panic!("error in GetMessageW"),
        }
    }
}

fn start(
    event_tx: Sender<(Position, CaptureEvent)>,
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
) -> (thread::JoinHandle<()>, u32) {
    /* condition variable to wait for thead id */
    let thread_id = Arc::new((Condvar::new(), Mutex::new(None)));
    let thread_id_ = Arc::clone(&thread_id);

    let msg_thread = thread::spawn(|| start_routine(thread_id_, event_tx, request_buffer));

    /* wait for thread to set its id */
    let (cond, thread_id) = &*thread_id;
    let mut thread_id = thread_id.lock().unwrap();
    while (*thread_id).is_none() {
        thread_id = cond.wait(thread_id).expect("channel closed");
    }
    (msg_thread, thread_id.expect("thread id"))
}

fn start_routine(
    ready: Arc<(Condvar, Mutex<Option<u32>>)>,
    event_tx: Sender<(Position, CaptureEvent)>,
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
) {
    EVENT_TX.replace(Some(event_tx));

    // Hook coordinates and display modes are physical pixels. Without this,
    // SetCursorPos/GetCursorPos use scaled coordinates on high-DPI displays
    // and edge entry lands at the wrong position.
    if unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
        .0
        .is_null()
    {
        log::warn!(
            "could not enable per-monitor DPI awareness; edge entry may be misplaced on scaled displays"
        );
    }

    /* communicate thread id */
    {
        let (cnd, mtx) = &*ready;
        let mut ready = mtx.lock().unwrap();
        *ready = Some(unsafe { GetCurrentThreadId() });
        cnd.notify_one();
    }

    let mouse_proc: HOOKPROC = Some(mouse_proc);
    let kybrd_proc: HOOKPROC = Some(kybrd_proc);
    let window_proc: WNDPROC = Some(window_proc);

    /* register hooks */
    unsafe {
        let _ = SetWindowsHookExW(WH_MOUSE_LL, mouse_proc, None, 0).unwrap();
        let _ = SetWindowsHookExW(WH_KEYBOARD_LL, kybrd_proc, None, 0).unwrap();
    }

    let instance = unsafe { GetModuleHandleW(None).unwrap() };
    let instance = instance.into();
    let window_class: WNDCLASSW = WNDCLASSW {
        lpfnWndProc: window_proc,
        hInstance: instance,
        lpszClassName: w!("lan-mouse-message-window-class"),
        ..Default::default()
    };

    static WINDOW_CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);
    if WINDOW_CLASS_REGISTERED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        /* register window class if not yet done so */
        unsafe {
            let ret = RegisterClassW(&window_class);
            if ret == 0 {
                panic!("RegisterClassW");
            }
        }
    }

    /* window is used ro receive WM_DISPLAYCHANGE messages */
    unsafe {
        CreateWindowExW(
            Default::default(),
            w!("lan-mouse-message-window-class"),
            w!("lan-mouse-msg-window"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
        .expect("CreateWindowExW");
    }

    /* run message loop */
    while let Some(msg) = get_msg() {
        // mouse / keybrd proc do not actually return a message
        if msg.hwnd.0.is_null() {
            /* messages sent via PostThreadMessage */
            match msg.wParam.0 {
                x if x == RequestType::Exit as usize => break,
                x if x == RequestType::Release as usize => {
                    ACTIVE_CLIENT.take();
                }
                x if x == RequestType::ClientUpdate as usize => {
                    let requests = {
                        let mut res = vec![];
                        let mut requests = request_buffer.lock().unwrap();
                        for request in requests.drain(..) {
                            res.push(request);
                        }
                        res
                    };

                    for request in requests {
                        update_clients(request)
                    }
                }
                _ => {}
            }
        } else {
            /* other messages for window_procs */
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

fn check_client_activation(wparam: WPARAM, lparam: LPARAM) -> bool {
    if wparam.0 != WM_MOUSEMOVE as usize {
        return ACTIVE_CLIENT.get().is_some();
    }
    let mouse_low_level: MSLLHOOKSTRUCT = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
    let curr_pos = (mouse_low_level.pt.x, mouse_low_level.pt.y);
    let prev_pos = PREV_POS.get().unwrap_or(curr_pos);
    PREV_POS.replace(Some(curr_pos));

    /* next event is the first actual event */
    let ret = ACTIVE_CLIENT.get().is_some();

    /* client already active, no need to check */
    if ACTIVE_CLIENT.get().is_some() {
        return ret;
    }

    /* check if a client was activated */
    let entered = DISPLAYS.with_borrow_mut(|(displays, generation)| {
        update_display_regions(displays, generation);
        display_util::entered_barrier(prev_pos, curr_pos, displays)
    });

    let Some(pos) = entered else {
        return ret;
    };

    /* check if a client is registered for the barrier */
    if !CLIENTS.with_borrow(|clients| clients.contains(&pos)) {
        return ret;
    }

    /* update active client and entry point */
    ACTIVE_CLIENT.replace(Some(pos));
    // a key-up the hook never saw (e.g. on the lock screen) would make that
    // key's presses look like auto-repeats for the whole session
    LOCAL_KEYS.with_borrow_mut(|keys| {
        keys.retain(|&vk| unsafe { GetAsyncKeyState(vk as i32) } < 0);
    });
    let entry_point = DISPLAYS.with_borrow(|(displays, _)| {
        display_util::clamp_to_display_bounds(displays, prev_pos, curr_pos)
    });
    ENTRY_POINT.replace(entry_point);

    /* notify main thread */
    log::debug!("ENTERED @ {prev_pos:?} -> {curr_pos:?}");
    let active = ACTIVE_CLIENT.get().expect("active client");
    let fraction = DISPLAYS.with_borrow(|(displays, _)| {
        let displays = display_util::rects(displays);
        input_event::edge::bounds(&displays).map(|b| {
            let fraction = input_event::edge::fraction(
                b,
                active.edge(),
                f64::from(entry_point.0),
                f64::from(entry_point.1),
            );
            let percent = f64::from(fraction) * 100.0 / f64::from(u16::MAX);
            log::info!("edge-position exit: backend=windows edge={active} position={percent:.3}% from top/left cursor={entry_point:?} previous={prev_pos:?} crossing={curr_pos:?} bounds={b:?}");
            fraction
        })
    });
    blocking_send_event(
        active,
        fraction
            .map(CaptureEvent::BeginAt)
            .unwrap_or(CaptureEvent::Begin),
    );

    ret
}

unsafe extern "system" fn mouse_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let active = check_client_activation(wparam, lparam);

    /* no client was active */
    if !active {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }

    /* get active client if any */
    let Some(pos) = ACTIVE_CLIENT.get() else {
        return LRESULT(1);
    };

    /* convert to lan-mouse event */
    let Some(pointer_event) = to_mouse_event(wparam, lparam) else {
        return LRESULT(1);
    };

    /* notify mainthread (drop events if sending too fast) */
    if let Err(e) = try_send_event(pos, CaptureEvent::Input(Event::Pointer(pointer_event))) {
        log::warn!("e: {e}");
    }

    /* don't pass event to applications */
    LRESULT(1)
}

/// What the keyboard hook does with a key event.
#[derive(Debug, PartialEq, Eq)]
enum KeyAction {
    /// let Windows handle it
    Pass,
    /// send it to the active client
    Forward,
    /// drop it
    Drop,
}

/// A key that went down while Windows had control must also go up on
/// Windows, even when it is released while a client is active. Otherwise
/// Windows keeps it pressed after control returns (NickReardon/lan-mouse#54).
/// The client never saw that key go down, so it gets neither the release nor
/// the auto-repeats of the still-held key.
fn key_action(local_keys: &mut HashSet<u32>, active: bool, vk: u32, down: bool) -> KeyAction {
    if !active {
        if down {
            local_keys.insert(vk);
        } else {
            local_keys.remove(&vk);
        }
        return KeyAction::Pass;
    }
    if !local_keys.contains(&vk) {
        return KeyAction::Forward;
    }
    if down {
        // auto-repeat: a new press always follows a release
        KeyAction::Drop
    } else {
        local_keys.remove(&vk);
        KeyAction::Pass
    }
}

/// `dwExtraInfo` of input injected by [`release_masked`], so the hook passes
/// it through untouched.
const INJECTED_MARKER: usize = 0x4c4d_5345;

/// Unassigned virtual key, pressed and released to mask a lone release.
const MASK_KEY: u16 = 0xE8;

/// Win and Alt act on a lone press and release (Start menu, menu bar).
fn acts_alone(vk: u32) -> bool {
    // VK_LWIN, VK_RWIN, VK_LMENU, VK_RMENU
    matches!(vk, 0x5B | 0x5C | 0xA4 | 0xA5)
}

/// Release a held Win or Alt key on Windows behind a no-op key, so Windows
/// does not take it for a tap. The original release must be dropped, since
/// injected input is processed after the hook returns.
fn release_masked(key: &KBDLLHOOKSTRUCT) {
    let input = |vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECTED_MARKER,
            },
        },
    };
    let mut release = KEYEVENTF_KEYUP;
    if key.flags.contains(LLKHF_EXTENDED) {
        release |= KEYEVENTF_EXTENDEDKEY;
    }
    let inputs = [
        input(MASK_KEY, 0, KEYBD_EVENT_FLAGS(0)),
        input(MASK_KEY, 0, KEYEVENTF_KEYUP),
        input(key.vkCode as u16, key.scanCode as u16, release),
    ];
    let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        log::warn!("could not release key {:#x} on Windows", key.vkCode);
    }
}

unsafe extern "system" fn kybrd_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let key = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
    if key.dwExtraInfo == INJECTED_MARKER {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }
    let down = match wparam.0 {
        p if p == WM_KEYDOWN as usize || p == WM_SYSKEYDOWN as usize => Some(true),
        p if p == WM_KEYUP as usize || p == WM_SYSKEYUP as usize => Some(false),
        _ => None,
    };
    if let Some(down) = down {
        let active = ACTIVE_CLIENT.get().is_some();
        match LOCAL_KEYS.with_borrow_mut(|keys| key_action(keys, active, key.vkCode, down)) {
            // a key held across the edge, released while a client is active
            KeyAction::Pass if active && acts_alone(key.vkCode) => {
                release_masked(&key);
                return LRESULT(1);
            }
            KeyAction::Pass => return CallNextHookEx(None, ncode, wparam, lparam),
            KeyAction::Drop => return LRESULT(1),
            KeyAction::Forward => {}
        }
    }

    /* get active client if any */
    let Some(client) = ACTIVE_CLIENT.get() else {
        return CallNextHookEx(None, ncode, wparam, lparam);
    };

    /* convert to key event */
    let Some(key_event) = to_key_event(wparam, lparam) else {
        return LRESULT(1);
    };

    if let Err(e) = try_send_event(client, CaptureEvent::Input(Event::Keyboard(key_event))) {
        log::warn!("e: {e}");
    }

    /* don't pass event to applications */
    LRESULT(1)
}

unsafe extern "system" fn window_proc(
    _hwnd: HWND,
    uint: u32,
    _wparam: WPARAM,
    _lparam: LPARAM,
) -> LRESULT {
    if uint == WM_DISPLAYCHANGE {
        log::debug!("display resolution changed");
        DISPLAY_RESOLUTION_GENERATION.fetch_add(1, Ordering::Release);
    }
    LRESULT(1)
}

static DISPLAY_RESOLUTION_GENERATION: AtomicI32 = AtomicI32::new(1);

fn update_display_regions(displays: &mut Vec<RECT>, generation: &mut i32) {
    let global_generation = DISPLAY_RESOLUTION_GENERATION.load(Ordering::Acquire);
    if *generation != global_generation {
        enumerate_displays(displays);
        log::debug!("displays: {displays:?}");
        *generation = global_generation;
    }
}

fn enumerate_displays(display_rects: &mut Vec<RECT>) {
    display_rects.clear();
    unsafe {
        let mut devices = vec![];
        for i in 0.. {
            let mut device: DISPLAY_DEVICEW = std::mem::zeroed();
            device.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            let ret = EnumDisplayDevicesW(None, i, &mut device, EDD_GET_DEVICE_INTERFACE_NAME);
            if ret == FALSE {
                break;
            }
            if device
                .StateFlags
                .contains(DISPLAY_DEVICE_ATTACHED_TO_DESKTOP)
            {
                devices.push(device.DeviceName);
            }
        }
        for device in devices {
            let mut dev_mode: DEVMODEW = std::mem::zeroed();
            dev_mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
            let ret = EnumDisplaySettingsW(
                PCWSTR::from_raw(&device as *const _),
                ENUM_CURRENT_SETTINGS,
                &mut dev_mode,
            );
            if ret == FALSE {
                log::warn!("no display mode");
            }

            let pos = dev_mode.Anonymous1.Anonymous2.dmPosition;
            let (x, y) = (pos.x, pos.y);
            let (width, height) = (dev_mode.dmPelsWidth, dev_mode.dmPelsHeight);

            display_rects.push(RECT {
                left: x,
                right: x + width as i32,
                top: y,
                bottom: y + height as i32,
            });
        }
    }
}

fn update_clients(request: ClientUpdate) {
    match request {
        ClientUpdate::ReleaseAt(pos, fraction, done) => {
            ACTIVE_CLIENT.take();
            let (point, bounds) = DISPLAYS.with_borrow_mut(|(displays, generation)| {
                update_display_regions(displays, generation);
                let displays = display_util::rects(displays);
                (
                    input_event::edge::entry(&displays, pos.edge(), fraction),
                    input_event::edge::bounds(&displays),
                )
            });
            let result = if let Some((x, y)) = point {
                let requested = f64::from(fraction) * 100.0 / f64::from(u16::MAX);
                log::info!(
                    "edge-position entry: backend=windows edge={pos} requested={requested:.3}% from top/left target=({x:.3}, {y:.3}) bounds={bounds:?}"
                );
                PREV_POS.replace(Some((x.round() as i32, y.round() as i32)));
                let result = unsafe {
                    windows::Win32::UI::WindowsAndMessaging::SetCursorPos(
                        x.round() as i32,
                        y.round() as i32,
                    )
                }
                .map_err(|e| super::CaptureError::Io(std::io::Error::other(e.to_string())));
                if result.is_ok() {
                    let mut actual = windows::Win32::Foundation::POINT::default();
                    match unsafe {
                        windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut actual)
                    } {
                        Ok(()) => {
                            if let Some(bounds) = bounds {
                                let mapped = f64::from(input_event::edge::fraction(
                                    bounds,
                                    pos.edge(),
                                    x,
                                    y,
                                )) * 100.0
                                    / f64::from(u16::MAX);
                                let observed = f64::from(input_event::edge::fraction(
                                    bounds,
                                    pos.edge(),
                                    f64::from(actual.x),
                                    f64::from(actual.y),
                                )) * 100.0
                                    / f64::from(u16::MAX);
                                log::info!(
                                    "edge-position entry-readback: backend=windows edge={pos} requested={requested:.3}% mapped={mapped:.3}% actual={observed:.3}% from top/left cursor=({}, {}) delta=({:.3}, {:.3})",
                                    actual.x,
                                    actual.y,
                                    f64::from(actual.x) - x,
                                    f64::from(actual.y) - y
                                );
                            }
                        }
                        Err(error) => log::warn!(
                            "edge-position entry-readback failed: backend=windows edge={pos}: {error}"
                        ),
                    }
                }
                result
            } else {
                Err(super::CaptureError::Io(std::io::Error::other(
                    "no desktop display for cursor entry",
                )))
            };
            let _ = done.send(result);
        }
        ClientUpdate::Create(pos) => {
            CLIENTS.with_borrow_mut(|clients| clients.insert(pos));
        }
        ClientUpdate::Destroy(pos) => {
            if let Some(active_pos) = ACTIVE_CLIENT.get() {
                if pos == active_pos {
                    let _ = ACTIVE_CLIENT.take();
                }
            }
            CLIENTS.with_borrow_mut(|clients| clients.remove(&pos));
        }
    }
}

fn to_key_event(wparam: WPARAM, lparam: LPARAM) -> Option<KeyboardEvent> {
    let kybrdllhookstruct: KBDLLHOOKSTRUCT = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
    let mut scan_code = kybrdllhookstruct.scanCode;
    log::trace!("scan_code: {scan_code}");
    if kybrdllhookstruct.flags.contains(LLKHF_EXTENDED) {
        scan_code |= 0xE000;
    }
    let Ok(win_scan_code) = scancode::Windows::try_from(scan_code) else {
        log::warn!("failed to translate to windows scancode: {scan_code}");
        return None;
    };
    log::trace!("windows_scan: {win_scan_code:?}");
    let Ok(linux_scan_code): Result<Linux, ()> = win_scan_code.try_into() else {
        log::warn!("failed to translate into linux scancode: {win_scan_code:?}");
        return None;
    };
    log::trace!("windows_scan: {linux_scan_code:?}");
    let scan_code = linux_scan_code as u32;
    match wparam {
        WPARAM(p) if p == WM_KEYDOWN as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 1,
        }),
        WPARAM(p) if p == WM_KEYUP as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 0,
        }),
        WPARAM(p) if p == WM_SYSKEYDOWN as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 1,
        }),
        WPARAM(p) if p == WM_SYSKEYUP as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 0,
        }),
        _ => None,
    }
}

fn to_mouse_event(wparam: WPARAM, lparam: LPARAM) -> Option<PointerEvent> {
    let mouse_low_level: MSLLHOOKSTRUCT = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
    match wparam {
        WPARAM(p) if p == WM_LBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_LEFT,
            state: 1,
        }),
        WPARAM(p) if p == WM_MBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_MIDDLE,
            state: 1,
        }),
        WPARAM(p) if p == WM_RBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_RIGHT,
            state: 1,
        }),
        WPARAM(p) if p == WM_LBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_LEFT,
            state: 0,
        }),
        WPARAM(p) if p == WM_MBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_MIDDLE,
            state: 0,
        }),
        WPARAM(p) if p == WM_RBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_RIGHT,
            state: 0,
        }),
        WPARAM(p) if p == WM_MOUSEMOVE as usize => {
            let (x, y) = (mouse_low_level.pt.x, mouse_low_level.pt.y);
            let (ex, ey) = ENTRY_POINT.get();
            let (dx, dy) = (x - ex, y - ey);
            let (dx, dy) = (dx as f64, dy as f64);
            Some(PointerEvent::Motion { time: 0, dx, dy })
        }
        WPARAM(p) if p == WM_MOUSEWHEEL as usize => Some(PointerEvent::AxisDiscrete120 {
            axis: 0,
            value: -(mouse_low_level.mouseData as i32 >> 16),
        }),
        WPARAM(p) if p == WM_XBUTTONDOWN as usize || p == WM_XBUTTONUP as usize => {
            let hb = mouse_low_level.mouseData >> 16;
            let button = match hb {
                1 => BTN_BACK,
                2 => BTN_FORWARD,
                _ => {
                    log::warn!("unknown mouse button");
                    return None;
                }
            };
            Some(PointerEvent::Button {
                time: 0,
                button,
                state: if p == WM_XBUTTONDOWN as usize { 1 } else { 0 },
            })
        }
        WPARAM(p) if p == WM_MOUSEHWHEEL as usize => Some(PointerEvent::AxisDiscrete120 {
            axis: 1, // Horizontal
            value: mouse_low_level.mouseData as i32 >> 16,
        }),
        w => {
            log::warn!("unknown mouse event: {w:?}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LWIN: u32 = 0x5B;
    const KEY_A: u32 = 0x41;

    #[test]
    fn key_held_across_the_edge_is_released_on_windows() {
        let mut keys = HashSet::new();
        // Win goes down on Windows, then control moves to a client
        assert_eq!(key_action(&mut keys, false, LWIN, true), KeyAction::Pass);
        // auto-repeats while held are not sent to the client
        assert_eq!(key_action(&mut keys, true, LWIN, true), KeyAction::Drop);
        // the release goes to Windows, not to the client
        assert_eq!(key_action(&mut keys, true, LWIN, false), KeyAction::Pass);
        // a new press belongs to the client
        assert_eq!(key_action(&mut keys, true, LWIN, true), KeyAction::Forward);
        assert_eq!(key_action(&mut keys, true, LWIN, false), KeyAction::Forward);
    }

    #[test]
    fn keys_pressed_during_capture_go_to_the_client() {
        let mut keys = HashSet::new();
        assert_eq!(key_action(&mut keys, true, KEY_A, true), KeyAction::Forward);
        assert_eq!(
            key_action(&mut keys, true, KEY_A, false),
            KeyAction::Forward
        );
        assert!(keys.is_empty());
    }

    #[test]
    fn only_win_and_alt_are_masked() {
        for vk in [0x5B, 0x5C, 0xA4, 0xA5] {
            assert!(acts_alone(vk));
        }
        // shift, ctrl, a
        for vk in [0xA0, 0xA2, KEY_A] {
            assert!(!acts_alone(vk));
        }
    }

    #[test]
    fn keys_released_on_windows_are_forgotten() {
        let mut keys = HashSet::new();
        assert_eq!(key_action(&mut keys, false, KEY_A, true), KeyAction::Pass);
        assert_eq!(key_action(&mut keys, false, KEY_A, false), KeyAction::Pass);
        assert_eq!(key_action(&mut keys, true, KEY_A, true), KeyAction::Forward);
    }
}
