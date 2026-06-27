use std::collections::HashSet;
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio::time::{Duration, MissedTickBehavior, interval};
use tracing::{error, info, warn};
use uuid::Uuid;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey,
    VK_BACK, VK_CAPITAL, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_INSERT,
    VK_LCONTROL, VK_LEFT, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_NEXT, VK_OEM_1, VK_OEM_2,
    VK_OEM_3, VK_OEM_4, VK_OEM_5, VK_OEM_6, VK_OEM_7, VK_OEM_102, VK_OEM_COMMA, VK_OEM_MINUS,
    VK_OEM_PERIOD, VK_OEM_PLUS, VK_PAUSE, VK_PRIOR, VK_RCONTROL, VK_RETURN, VK_RIGHT, VK_RMENU,
    VK_RSHIFT, VK_RWIN, VK_SCROLL, VK_SHIFT, VK_SNAPSHOT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT,
    RIDEV_DEVNOTIFY, RIDEV_INPUTSINK, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, ClipCursor, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetCursorPos, GetMessageW, GetSystemMetrics, HC_ACTION, HHOOK, HWND_MESSAGE, KBDLLHOOKSTRUCT,
    MSG, PostQuitMessage, RI_KEY_BREAK, RI_MOUSE_BUTTON_1_DOWN, RI_MOUSE_BUTTON_1_UP,
    RI_MOUSE_BUTTON_2_DOWN, RI_MOUSE_BUTTON_2_UP, RI_MOUSE_BUTTON_3_DOWN, RI_MOUSE_BUTTON_3_UP,
    RI_MOUSE_BUTTON_4_DOWN, RI_MOUSE_BUTTON_4_UP, RI_MOUSE_BUTTON_5_DOWN, RI_MOUSE_BUTTON_5_UP,
    RI_MOUSE_HWHEEL, RI_MOUSE_WHEEL, RegisterClassW, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SetCursorPos, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WINDOW_EX_STYLE, WINDOW_STYLE, WM_DESTROY, WM_HOTKEY, WM_INPUT, WM_KEYDOWN,
    WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW,
};
use windows::core::w;

use crate::protocol::{
    ClientControlEvent, Frame, HostStateEvent, InputEvent, KeyCode, KeyState, Message, Modifier,
    MouseButton, ProtocolHello,
};
use crate::transport::FrameWriter;

const HOTKEY_ID_TOGGLE_BACKSLASH: i32 = 1;
const HOTKEY_ID_TOGGLE_NON_US_BACKSLASH: i32 = 2;
const EDGE_TRIGGER_PX: i32 = 8;
const HOTKEY_DEBOUNCE_MS: u64 = 250;
const MOUSE_FLUSH_INTERVAL_MS: u64 = 4;
const WHEEL_DELTA: i32 = 120;
const SCANCODE_BACKSLASH: u32 = 0x2b;
const SCANCODE_NON_US_BACKSLASH: u32 = 0x56;

static HOST_STATE: OnceLock<Mutex<HostState>> = OnceLock::new();

pub async fn run_host(peer: String, layout: String) -> Result<()> {
    if layout != "mac-left" {
        bail!("only --layout mac-left is implemented right now");
    }

    let stream = TcpStream::connect(&peer)
        .await
        .with_context(|| format!("connect {peer}"))?;
    stream.set_nodelay(true).context("set TCP_NODELAY")?;
    let (tx, rx) = mpsc::unbounded_channel();
    HOST_STATE
        .set(Mutex::new(HostState::new(tx, layout.clone())))
        .map_err(|_| anyhow!("Windows host state was already initialized"))?;

    let (read_half, write_half) = stream.into_split();
    tokio::spawn(writer_task(write_half, rx));
    tokio::spawn(control_reader_task(read_half));

    info!(%peer, %layout, "starting Windows host capture");
    run_message_loop().context("run Windows host message loop")
}

async fn writer_task(stream: OwnedWriteHalf, mut rx: mpsc::UnboundedReceiver<HostCommand>) {
    let mut writer = FrameWriter::new(stream);
    let session_id = Uuid::new_v4();
    let mut seq = 1_u64;

    if !write_host_message(
        &mut writer,
        session_id,
        &mut seq,
        Message::Hello(ProtocolHello {
            protocol_version: crate::protocol::PROTOCOL_VERSION,
            role: "windows-host".to_owned(),
            device_name: std::env::var("COMPUTERNAME")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "windows".to_owned()),
        }),
        "host hello failed",
    )
    .await
    {
        return;
    }

    let mut pending_dx = 0_i32;
    let mut pending_dy = 0_i32;
    let mut flush_timer = interval(Duration::from_millis(MOUSE_FLUSH_INTERVAL_MS));
    flush_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            command = rx.recv() => {
                let Some(command) = command else {
                    if !flush_pending_motion(&mut writer, session_id, &mut seq, &mut pending_dx, &mut pending_dy).await {
                        break;
                    }
                    break;
                };

                match command {
                    HostCommand::Input(InputEvent::MouseMotion { dx, dy }) => {
                        pending_dx += dx;
                        pending_dy += dy;
                    }
                    other => {
                        if !flush_pending_motion(&mut writer, session_id, &mut seq, &mut pending_dx, &mut pending_dy).await {
                            break;
                        }
                        if !write_host_message(
                            &mut writer,
                            session_id,
                            &mut seq,
                            message_from_host_command(other),
                            "host writer disconnected",
                        )
                        .await {
                            break;
                        }
                    }
                }
            }
            _ = flush_timer.tick(), if pending_dx != 0 || pending_dy != 0 => {
                if !flush_pending_motion(&mut writer, session_id, &mut seq, &mut pending_dx, &mut pending_dy).await {
                    break;
                }
            }
        }
    }
}

async fn flush_pending_motion(
    writer: &mut FrameWriter<OwnedWriteHalf>,
    session_id: Uuid,
    seq: &mut u64,
    pending_dx: &mut i32,
    pending_dy: &mut i32,
) -> bool {
    if *pending_dx == 0 && *pending_dy == 0 {
        return true;
    }

    let dx = std::mem::take(pending_dx);
    let dy = std::mem::take(pending_dy);
    write_host_message(
        writer,
        session_id,
        seq,
        Message::Input(InputEvent::MouseMotion { dx, dy }),
        "host writer disconnected",
    )
    .await
}

async fn write_host_message(
    writer: &mut FrameWriter<OwnedWriteHalf>,
    session_id: Uuid,
    seq: &mut u64,
    message: Message,
    disconnect_reason: &'static str,
) -> bool {
    if let Err(err) = writer
        .write_frame(Frame::new(session_id, *seq, message))
        .await
    {
        error!(?err, "host writer disconnected");
        release_host_state_after_transport_loss(disconnect_reason);
        return false;
    }
    *seq += 1;
    true
}

fn message_from_host_command(command: HostCommand) -> Message {
    match command {
        HostCommand::HostState { active, reason } => Message::HostState(HostStateEvent {
            remote_active: active,
            reason: reason.to_owned(),
            entry_x_ratio: None,
            entry_y_ratio: None,
        }),
        HostCommand::HostStateWithEntry {
            active,
            reason,
            entry_x_ratio,
            entry_y_ratio,
        } => Message::HostState(HostStateEvent {
            remote_active: active,
            reason: reason.to_owned(),
            entry_x_ratio: Some(entry_x_ratio),
            entry_y_ratio: Some(entry_y_ratio),
        }),
        HostCommand::Input(event) => Message::Input(event),
        HostCommand::Reset => Message::InputReset,
    }
}

async fn control_reader_task(stream: OwnedReadHalf) {
    let mut reader = crate::transport::FrameReader::new(stream);

    loop {
        let frame = match reader.read_frame().await {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                warn!("host control reader disconnected");
                release_host_state_after_transport_loss("control reader disconnected");
                break;
            }
            Err(err) => {
                warn!(?err, "host control reader failed");
                release_host_state_after_transport_loss("control reader failed");
                break;
            }
        };

        if let Message::ClientControl(ClientControlEvent::ReleaseHost {
            reason,
            entry_x_ratio,
            entry_y_ratio,
        }) = frame.message
        {
            info!(%reason, "client requested host release");
            match lock_state() {
                Ok(mut state) => {
                    if state.remote_active
                        && let Err(err) = state.set_remote_active(
                            false,
                            "mac right edge",
                            entry_x_ratio,
                            entry_y_ratio,
                        )
                    {
                        warn!(?err, "failed to release host from client control");
                    }
                }
                Err(err) => warn!(?err, "failed to lock host state for client control"),
            }
        }
    }
}

fn run_message_loop() -> Result<()> {
    let hwnd = unsafe { create_message_window()? };
    let setup = unsafe { register_input(hwnd).and_then(|_| register_hotkey(hwnd)) }
        .and_then(|_| install_persistent_keyboard_hook());

    if let Err(err) = setup {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        return Err(err);
    }

    info!("Windows Raw Input host is ready; Ctrl+Alt+\\ toggles remote control");

    let mut msg = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if result.0 == -1 {
            unsafe {
                cleanup_host_state();
                unregister_hotkeys(hwnd);
                let _ = DestroyWindow(hwnd);
            }
            bail!("GetMessageW failed");
        }
        if result.0 == 0 {
            break;
        }

        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    unsafe {
        cleanup_host_state();
        unregister_hotkeys(hwnd);
        let _ = DestroyWindow(hwnd);
    }
    Ok(())
}

unsafe fn create_message_window() -> Result<HWND> {
    let module = unsafe { GetModuleHandleW(None).context("GetModuleHandleW")? };
    let hinstance = HINSTANCE(module.0);
    let class_name = w!("SoftKvmHostMessageWindow");
    let class = WNDCLASSW {
        hInstance: hinstance,
        lpszClassName: class_name,
        lpfnWndProc: Some(wnd_proc),
        ..Default::default()
    };

    let atom = unsafe { RegisterClassW(&class) };
    if atom == 0 {
        return Err(windows::core::Error::from_thread()).context("RegisterClassW");
    }

    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("softkvm"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinstance),
            None,
        )
    }
    .context("CreateWindowExW")
}

unsafe fn register_input(hwnd: HWND) -> Result<()> {
    let flags = RIDEV_INPUTSINK | RIDEV_DEVNOTIFY;
    let devices = [
        RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x02,
            dwFlags: flags,
            hwndTarget: hwnd,
        },
        RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x06,
            dwFlags: flags,
            hwndTarget: hwnd,
        },
    ];

    unsafe {
        RegisterRawInputDevices(&devices, size_of::<RAWINPUTDEVICE>() as u32)
            .context("RegisterRawInputDevices")
    }
}

unsafe fn register_hotkey(hwnd: HWND) -> Result<()> {
    let modifiers = MOD_CONTROL | MOD_ALT | MOD_NOREPEAT;

    let mut registered = false;
    for (id, vkey, name) in [
        (HOTKEY_ID_TOGGLE_BACKSLASH, VK_OEM_5.0, "VK_OEM_5"),
        (
            HOTKEY_ID_TOGGLE_NON_US_BACKSLASH,
            VK_OEM_102.0,
            "VK_OEM_102",
        ),
    ] {
        match unsafe { RegisterHotKey(Some(hwnd), id, modifiers, u32::from(vkey)) } {
            Ok(()) => registered = true,
            Err(err) => warn!(?err, name, "RegisterHotKey Ctrl+Alt+\\ failed"),
        }
    }

    if !registered {
        warn!("RegisterHotKey Ctrl+Alt+\\ failed; low-level keyboard hook will handle the hotkey");
    }
    Ok(())
}

unsafe fn unregister_hotkeys(hwnd: HWND) {
    let _ = unsafe { UnregisterHotKey(Some(hwnd), HOTKEY_ID_TOGGLE_BACKSLASH) };
    let _ = unsafe { UnregisterHotKey(Some(hwnd), HOTKEY_ID_TOGGLE_NON_US_BACKSLASH) };
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_HOTKEY
            if matches!(
                wparam.0 as i32,
                HOTKEY_ID_TOGGLE_BACKSLASH | HOTKEY_ID_TOGGLE_NON_US_BACKSLASH
            ) =>
        {
            if let Err(err) = toggle_remote("hotkey Ctrl+(Alt|Win)+\\") {
                error!(?err, "toggle failed");
            }
            LRESULT(0)
        }
        WM_INPUT => {
            if let Err(err) = handle_raw_input(lparam) {
                warn!(?err, "raw input failed");
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_DESTROY => {
            unsafe {
                cleanup_host_state();
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn handle_raw_input(lparam: LPARAM) -> Result<()> {
    let raw = unsafe { read_raw_input(lparam)? };
    match raw.header.dwType {
        t if t == RIM_TYPEMOUSE.0 => handle_mouse(raw),
        t if t == RIM_TYPEKEYBOARD.0 => handle_keyboard(raw),
        _ => Ok(()),
    }
}

unsafe fn read_raw_input(lparam: LPARAM) -> Result<RAWINPUT> {
    let handle = HRAWINPUT(lparam.0 as *mut c_void);
    let mut size = 0_u32;
    let header_size = size_of::<RAWINPUTHEADER>() as u32;

    let probe = unsafe { GetRawInputData(handle, RID_INPUT, None, &mut size, header_size) };
    if probe == u32::MAX {
        return Err(windows::core::Error::from_thread()).context("GetRawInputData size");
    }

    let mut buffer_size = size.max(size_of::<RAWINPUT>() as u32);
    let mut data = vec![0_u8; buffer_size as usize];
    let read = unsafe {
        GetRawInputData(
            handle,
            RID_INPUT,
            Some(data.as_mut_ptr().cast::<c_void>()),
            &mut buffer_size,
            header_size,
        )
    };
    if read == u32::MAX {
        return Err(windows::core::Error::from_thread()).context("GetRawInputData body");
    }

    Ok(unsafe { std::ptr::read_unaligned(data.as_ptr().cast::<RAWINPUT>()) })
}

fn handle_mouse(raw: RAWINPUT) -> Result<()> {
    let mouse = unsafe { raw.data.mouse };
    let mut events = Vec::new();

    if mouse.lLastX != 0 || mouse.lLastY != 0 {
        events.push(InputEvent::MouseMotion {
            dx: mouse.lLastX,
            dy: mouse.lLastY,
        });
    }

    let buttons = unsafe { mouse.Anonymous.Anonymous };
    let flags = u32::from(buttons.usButtonFlags);
    push_mouse_button_events(&mut events, flags);

    if flags & RI_MOUSE_WHEEL != 0 {
        let dy = wheel_units(buttons.usButtonData);
        if dy != 0 {
            events.push(InputEvent::MouseWheel { dx: 0, dy });
        }
    }
    if flags & RI_MOUSE_HWHEEL != 0 {
        let dx = wheel_units(buttons.usButtonData);
        if dx != 0 {
            events.push(InputEvent::MouseWheel { dx, dy: 0 });
        }
    }

    let mut state = lock_state()?;
    if !state.remote_active
        && state.layout == "mac-left"
        && mouse.lLastX < 0
        && cursor_at_left_edge()
    {
        let (_, y_ratio) = current_monitor_cursor_ratios().unwrap_or((0.98, 0.5));
        state.set_remote_active(true, "left edge", Some(0.98), Some(y_ratio))?;
    }

    if state.remote_active {
        for event in events {
            state.send(HostCommand::Input(event))?;
        }
    }

    Ok(())
}

fn push_mouse_button_events(events: &mut Vec<InputEvent>, flags: u32) {
    for (mask, button, state) in [
        (RI_MOUSE_BUTTON_1_DOWN, MouseButton::Left, KeyState::Down),
        (RI_MOUSE_BUTTON_1_UP, MouseButton::Left, KeyState::Up),
        (RI_MOUSE_BUTTON_2_DOWN, MouseButton::Right, KeyState::Down),
        (RI_MOUSE_BUTTON_2_UP, MouseButton::Right, KeyState::Up),
        (RI_MOUSE_BUTTON_3_DOWN, MouseButton::Middle, KeyState::Down),
        (RI_MOUSE_BUTTON_3_UP, MouseButton::Middle, KeyState::Up),
        (RI_MOUSE_BUTTON_4_DOWN, MouseButton::Back, KeyState::Down),
        (RI_MOUSE_BUTTON_4_UP, MouseButton::Back, KeyState::Up),
        (RI_MOUSE_BUTTON_5_DOWN, MouseButton::Forward, KeyState::Down),
        (RI_MOUSE_BUTTON_5_UP, MouseButton::Forward, KeyState::Up),
    ] {
        if flags & mask != 0 {
            events.push(InputEvent::MouseButton { button, state });
        }
    }
}

fn wheel_units(raw: u16) -> i32 {
    let delta = i32::from(raw as i16);
    if delta.abs() >= WHEEL_DELTA {
        delta / WHEEL_DELTA
    } else {
        delta.signum()
    }
}

fn handle_keyboard(raw: RAWINPUT) -> Result<()> {
    let keyboard = unsafe { raw.data.keyboard };
    let key_state = if u32::from(keyboard.Flags) & RI_KEY_BREAK != 0 {
        KeyState::Up
    } else {
        KeyState::Down
    };
    let vkey = keyboard.VKey;

    let mut state = lock_state()?;
    if state.remote_active {
        return Ok(());
    }

    state.keyboard_events(vkey, key_state);
    Ok(())
}

fn toggle_remote(reason: &'static str) -> Result<()> {
    let mut state = lock_state()?;
    if !state.accept_hotkey_toggle() {
        return Ok(());
    }
    let active = !state.remote_active;
    state.set_remote_active(active, reason, Some(0.5), Some(0.5))
}

fn cursor_at_left_edge() -> bool {
    unsafe {
        let mut point = Default::default();
        if GetCursorPos(&mut point).is_err() {
            return false;
        }
        point.x <= GetSystemMetrics(SM_XVIRTUALSCREEN) + EDGE_TRIGGER_PX
    }
}

fn current_monitor_cursor_ratios() -> Option<(f64, f64)> {
    let point = current_cursor_position().ok()?;
    let rect = monitor_rect_for_point(point)?;
    let width = (rect.right - rect.left).max(1) as f64;
    let height = (rect.bottom - rect.top).max(1) as f64;
    let x = ((point.x - rect.left) as f64 / width).clamp(0.0, 1.0);
    let y = ((point.y - rect.top) as f64 / height).clamp(0.0, 1.0);
    Some((x, y))
}

fn monitor_rect_for_point(point: POINT) -> Option<RECT> {
    let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return None;
    }
    Some(info.rcMonitor)
}

fn point_from_rect_ratios(rect: RECT, x_ratio: f64, y_ratio: f64) -> POINT {
    let inset = 16.0;
    let width = (rect.right - rect.left).max(1) as f64;
    let height = (rect.bottom - rect.top).max(1) as f64;
    POINT {
        x: (rect.left as f64 + (width - inset * 2.0) * x_ratio.clamp(0.0, 1.0) + inset).round()
            as i32,
        y: (rect.top as f64 + (height - inset * 2.0) * y_ratio.clamp(0.0, 1.0) + inset).round()
            as i32,
    }
}

unsafe fn release_remote_controls() {
    let _ = unsafe { ClipCursor(None) };
}

unsafe fn cleanup_host_state() {
    if let Some(state) = HOST_STATE.get()
        && let Ok(mut state) = state.lock()
    {
        state.release_local_controls(true, None, None);
        state.uninstall_hooks();
        state.remote_active = false;
        return;
    }

    unsafe {
        release_remote_controls();
    }
}

fn release_host_state_after_transport_loss(reason: &'static str) {
    match lock_state() {
        Ok(mut state) => {
            if state.remote_active {
                warn!(reason, "transport lost; releasing local Windows controls");
            }
            state.remote_active = false;
            state.pressed_modifier_keys.clear();
            state.active_modifiers.clear();
            state.release_local_controls(true, None, None);
        }
        Err(err) => warn!(
            ?err,
            reason, "failed to release local controls after transport loss"
        ),
    }
}

fn install_persistent_keyboard_hook() -> Result<()> {
    let mut state = lock_state()?;
    state.install_keyboard_hook()?;
    info!("persistent Windows keyboard hook is ready for Ctrl+(Alt|Win)+\\");
    Ok(())
}

unsafe extern "system" fn mouse_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && remote_is_active() {
        return LRESULT(1);
    }

    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn keyboard_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let message = wparam.0 as u32;
        let key_down_message = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
        let key_up_message = message == WM_KEYUP || message == WM_SYSKEYUP;

        if key_down_message || key_up_message {
            let key = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };

            if key_down_message {
                let ctrl_toggle = ctrl_toggle_modifier_down();
                if remote_is_active() && ctrl_toggle {
                    info!(
                        vkey = key.vkCode,
                        scan_code = key.scanCode,
                        "remote Ctrl+toggle-modifier keydown"
                    );
                }
                if is_backslash_key(key.vkCode as u16, key.scanCode) && ctrl_toggle {
                    if let Err(err) = toggle_remote("hotkey Ctrl+(Alt|Win)+\\") {
                        error!(?err, "toggle failed");
                    }
                    return LRESULT(1);
                }
            }

            if remote_is_active() {
                let key_state = if key_up_message {
                    KeyState::Up
                } else {
                    KeyState::Down
                };
                if let Err(err) =
                    send_remote_keyboard_from_hook(key.vkCode as u16, key.scanCode, key_state)
                {
                    warn!(?err, "failed to send remote keyboard hook input");
                }
                return LRESULT(1);
            }
        }

        if remote_is_active() {
            return LRESULT(1);
        }
    }

    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn send_remote_keyboard_from_hook(vkey: u16, scan_code: u32, key_state: KeyState) -> Result<()> {
    let mut state = lock_state()?;
    if !state.remote_active {
        return Ok(());
    }

    let events = state.keyboard_events(vkey, key_state);
    if !events.is_empty() {
        info!(
            vkey,
            scan_code,
            ?key_state,
            ?events,
            "sending remote keyboard hook input"
        );
    }
    for event in events {
        state.send(HostCommand::Input(event))?;
    }
    Ok(())
}

fn remote_is_active() -> bool {
    HOST_STATE
        .get()
        .and_then(|state| state.lock().ok().map(|state| state.remote_active))
        .unwrap_or(false)
}

fn ctrl_toggle_modifier_down() -> bool {
    (key_down(VK_CONTROL.0) || key_down(VK_LCONTROL.0) || key_down(VK_RCONTROL.0))
        && (key_down(VK_MENU.0)
            || key_down(VK_LMENU.0)
            || key_down(VK_RMENU.0)
            || key_down(VK_LWIN.0)
            || key_down(VK_RWIN.0))
}

fn key_down(vkey: u16) -> bool {
    unsafe { GetAsyncKeyState(i32::from(vkey)) < 0 }
}

fn is_backslash_key(vkey: u16, scan_code: u32) -> bool {
    vkey == VK_OEM_5.0
        || vkey == VK_OEM_102.0
        || scan_code == SCANCODE_BACKSLASH
        || scan_code == SCANCODE_NON_US_BACKSLASH
}

fn lock_state() -> Result<std::sync::MutexGuard<'static, HostState>> {
    HOST_STATE
        .get()
        .ok_or_else(|| anyhow!("Windows host state is not initialized"))?
        .lock()
        .map_err(|_| anyhow!("Windows host state lock poisoned"))
}

#[derive(Clone, Debug)]
enum HostCommand {
    HostState {
        active: bool,
        reason: &'static str,
    },
    HostStateWithEntry {
        active: bool,
        reason: &'static str,
        entry_x_ratio: f64,
        entry_y_ratio: f64,
    },
    Input(InputEvent),
    Reset,
}

struct HostState {
    tx: mpsc::UnboundedSender<HostCommand>,
    remote_active: bool,
    layout: String,
    pressed_modifier_keys: HashSet<u16>,
    active_modifiers: HashSet<Modifier>,
    mouse_hook: Option<isize>,
    keyboard_hook: Option<isize>,
    saved_cursor_pos: Option<POINT>,
    saved_monitor_rect: Option<RECT>,
    last_hotkey_toggle: Option<Instant>,
}

impl HostState {
    fn new(tx: mpsc::UnboundedSender<HostCommand>, layout: String) -> Self {
        Self {
            tx,
            remote_active: false,
            layout,
            pressed_modifier_keys: HashSet::new(),
            active_modifiers: HashSet::new(),
            mouse_hook: None,
            keyboard_hook: None,
            saved_cursor_pos: None,
            saved_monitor_rect: None,
            last_hotkey_toggle: None,
        }
    }

    fn accept_hotkey_toggle(&mut self) -> bool {
        let now = Instant::now();
        if self.last_hotkey_toggle.is_some_and(|last| {
            now.duration_since(last) < Duration::from_millis(HOTKEY_DEBOUNCE_MS)
        }) {
            return false;
        }
        self.last_hotkey_toggle = Some(now);
        true
    }

    fn set_remote_active(
        &mut self,
        active: bool,
        reason: &'static str,
        entry_x_ratio: Option<f64>,
        entry_y_ratio: Option<f64>,
    ) -> Result<()> {
        if self.remote_active == active {
            return Ok(());
        }

        self.pressed_modifier_keys.clear();
        self.active_modifiers.clear();

        if active {
            self.capture_local_controls()?;
            self.remote_active = true;
            match entry_x_ratio.zip(entry_y_ratio) {
                Some((entry_x_ratio, entry_y_ratio)) => {
                    self.send(HostCommand::HostStateWithEntry {
                        active,
                        reason,
                        entry_x_ratio,
                        entry_y_ratio,
                    })?;
                }
                None => self.send(HostCommand::HostState { active, reason })?,
            }
            info!(reason, "remote macOS control enabled");
        } else {
            self.remote_active = false;
            self.release_local_controls(true, entry_x_ratio, entry_y_ratio);
            self.send(HostCommand::HostState { active, reason })?;
            self.send(HostCommand::Reset)?;
            info!(reason, "remote macOS control disabled");
        }
        Ok(())
    }

    fn send(&self, command: HostCommand) -> Result<()> {
        self.tx
            .send(command)
            .map_err(|_| anyhow!("host writer task is gone"))
    }

    fn keyboard_events(&mut self, vkey: u16, key_state: KeyState) -> Vec<InputEvent> {
        if let Some(modifier) = modifier_for_vkey(vkey) {
            let was_active = self.active_modifiers.contains(&modifier);
            match key_state {
                KeyState::Down => {
                    self.pressed_modifier_keys.insert(vkey);
                }
                KeyState::Up => {
                    self.pressed_modifier_keys.remove(&vkey);
                }
            }
            let is_active = self
                .pressed_modifier_keys
                .iter()
                .copied()
                .any(|pressed| modifier_for_vkey(pressed) == Some(modifier));

            if was_active != is_active {
                if is_active {
                    self.active_modifiers.insert(modifier);
                    vec![InputEvent::Modifier {
                        modifier,
                        state: KeyState::Down,
                    }]
                } else {
                    self.active_modifiers.remove(&modifier);
                    vec![InputEvent::Modifier {
                        modifier,
                        state: KeyState::Up,
                    }]
                }
            } else {
                Vec::new()
            }
        } else {
            vec![InputEvent::Key {
                key: key_for_vkey(vkey),
                state: key_state,
            }]
        }
    }

    fn capture_local_controls(&mut self) -> Result<()> {
        let cursor = current_cursor_position().unwrap_or_else(|_| POINT {
            x: unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) },
            y: unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) },
        });
        self.saved_cursor_pos = Some(cursor);
        self.saved_monitor_rect = monitor_rect_for_point(cursor);
        self.install_hooks()?;
        unsafe {
            clip_cursor_to_point(cursor)?;
            SetCursorPos(cursor.x, cursor.y).context("SetCursorPos freeze")?;
        }
        Ok(())
    }

    fn release_local_controls(
        &mut self,
        restore_cursor: bool,
        entry_x_ratio: Option<f64>,
        entry_y_ratio: Option<f64>,
    ) {
        unsafe {
            release_remote_controls();
        }
        self.uninstall_mouse_hook();

        if restore_cursor {
            let target = match (self.saved_monitor_rect, entry_x_ratio.zip(entry_y_ratio)) {
                (Some(rect), Some((x_ratio, y_ratio))) => {
                    Some(point_from_rect_ratios(rect, x_ratio, y_ratio))
                }
                _ => self.saved_cursor_pos,
            };

            if let Some(cursor) = target
                && let Err(err) = unsafe { SetCursorPos(cursor.x, cursor.y) }
            {
                warn!(?err, "failed to restore Windows cursor position");
            }
        }
        self.saved_cursor_pos = None;
        self.saved_monitor_rect = None;
    }

    fn install_hooks(&mut self) -> Result<()> {
        self.install_mouse_hook()?;
        self.install_keyboard_hook()
    }

    fn install_mouse_hook(&mut self) -> Result<()> {
        if self.mouse_hook.is_none() {
            let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), None, 0) }
                .context("SetWindowsHookExW WH_MOUSE_LL")?;
            self.mouse_hook = Some(hook.0 as isize);
        }
        Ok(())
    }

    fn install_keyboard_hook(&mut self) -> Result<()> {
        if self.keyboard_hook.is_none() {
            let hook =
                unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), None, 0) }
                    .context("SetWindowsHookExW WH_KEYBOARD_LL")?;
            self.keyboard_hook = Some(hook.0 as isize);
        }

        Ok(())
    }

    fn uninstall_mouse_hook(&mut self) {
        if let Some(hook) = self.mouse_hook.take() {
            uninstall_hook(hook, "WH_MOUSE_LL");
        }
    }

    fn uninstall_keyboard_hook(&mut self) {
        if let Some(hook) = self.keyboard_hook.take() {
            uninstall_hook(hook, "WH_KEYBOARD_LL");
        }
    }

    fn uninstall_hooks(&mut self) {
        self.uninstall_mouse_hook();
        self.uninstall_keyboard_hook();
    }
}

fn current_cursor_position() -> Result<POINT> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point).context("GetCursorPos")? };
    Ok(point)
}

unsafe fn clip_cursor_to_point(point: POINT) -> Result<()> {
    let rect = RECT {
        left: point.x,
        top: point.y,
        right: point.x + 1,
        bottom: point.y + 1,
    };
    unsafe { ClipCursor(Some(&rect)).context("ClipCursor saved cursor point") }
}

fn uninstall_hook(handle: isize, name: &'static str) {
    let hook = HHOOK(handle as *mut c_void);
    if let Err(err) = unsafe { UnhookWindowsHookEx(hook) } {
        warn!(?err, name, "failed to uninstall Windows hook");
    }
}

fn modifier_for_vkey(vkey: u16) -> Option<Modifier> {
    match vkey {
        v if v == VK_CONTROL.0 || v == VK_LCONTROL.0 || v == VK_RCONTROL.0 => {
            Some(Modifier::Control)
        }
        v if v == VK_MENU.0 || v == VK_LMENU.0 || v == VK_RMENU.0 => Some(Modifier::Alt),
        v if v == VK_LWIN.0 || v == VK_RWIN.0 => Some(Modifier::Super),
        v if v == VK_SHIFT.0 || v == VK_LSHIFT.0 || v == VK_RSHIFT.0 => Some(Modifier::Shift),
        _ => None,
    }
}

fn key_for_vkey(vkey: u16) -> KeyCode {
    if let Some(usage) = usb_usage_for_vkey(vkey) {
        match usage {
            0x28 => KeyCode::Enter,
            0x29 => KeyCode::Escape,
            0x2b => KeyCode::Tab,
            0x2c => KeyCode::Space,
            0x31 => KeyCode::Backslash,
            _ => KeyCode::Usb(usage),
        }
    } else {
        KeyCode::Other(u32::from(vkey))
    }
}

fn usb_usage_for_vkey(vkey: u16) -> Option<u16> {
    if (0x41..=0x5a).contains(&vkey) {
        return Some(0x04 + (vkey - 0x41));
    }
    if (0x31..=0x39).contains(&vkey) {
        return Some(0x1e + (vkey - 0x31));
    }
    if vkey == 0x30 {
        return Some(0x27);
    }
    if (0x70..=0x7b).contains(&vkey) {
        return Some(0x3a + (vkey - 0x70));
    }

    match vkey {
        v if v == VK_RETURN.0 => Some(0x28),
        v if v == VK_ESCAPE.0 => Some(0x29),
        v if v == VK_BACK.0 => Some(0x2a),
        v if v == VK_TAB.0 => Some(0x2b),
        v if v == VK_SPACE.0 => Some(0x2c),
        v if v == VK_OEM_MINUS.0 => Some(0x2d),
        v if v == VK_OEM_PLUS.0 => Some(0x2e),
        v if v == VK_OEM_4.0 => Some(0x2f),
        v if v == VK_OEM_6.0 => Some(0x30),
        v if v == VK_OEM_5.0 => Some(0x31),
        v if v == VK_OEM_1.0 => Some(0x33),
        v if v == VK_OEM_7.0 => Some(0x34),
        v if v == VK_OEM_3.0 => Some(0x35),
        v if v == VK_OEM_COMMA.0 => Some(0x36),
        v if v == VK_OEM_PERIOD.0 => Some(0x37),
        v if v == VK_OEM_2.0 => Some(0x38),
        v if v == VK_CAPITAL.0 => Some(0x39),
        v if v == VK_SNAPSHOT.0 => Some(0x46),
        v if v == VK_SCROLL.0 => Some(0x47),
        v if v == VK_PAUSE.0 => Some(0x48),
        v if v == VK_INSERT.0 => Some(0x49),
        v if v == VK_HOME.0 => Some(0x4a),
        v if v == VK_PRIOR.0 => Some(0x4b),
        v if v == VK_DELETE.0 => Some(0x4c),
        v if v == VK_END.0 => Some(0x4d),
        v if v == VK_NEXT.0 => Some(0x4e),
        v if v == VK_RIGHT.0 => Some(0x4f),
        v if v == VK_LEFT.0 => Some(0x50),
        v if v == VK_DOWN.0 => Some(0x51),
        v if v == VK_UP.0 => Some(0x52),
        v if v == VK_OEM_102.0 => Some(0x64),
        _ => None,
    }
}
