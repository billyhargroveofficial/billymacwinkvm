use std::collections::HashSet;
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, anyhow, bail};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use uuid::Uuid;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey, VK_CONTROL, VK_ESCAPE,
    VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_OEM_5, VK_RCONTROL, VK_RETURN, VK_RMENU,
    VK_RSHIFT, VK_RWIN, VK_SHIFT, VK_SPACE, VK_TAB,
};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT,
    RIDEV_DEVNOTIFY, RIDEV_INPUTSINK, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    ClipCursor, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos,
    GetMessageW, GetSystemMetrics, HWND_MESSAGE, MSG, PostQuitMessage, RI_KEY_BREAK,
    RI_MOUSE_BUTTON_1_DOWN, RI_MOUSE_BUTTON_1_UP, RI_MOUSE_BUTTON_2_DOWN, RI_MOUSE_BUTTON_2_UP,
    RI_MOUSE_BUTTON_3_DOWN, RI_MOUSE_BUTTON_3_UP, RI_MOUSE_BUTTON_4_DOWN, RI_MOUSE_BUTTON_4_UP,
    RI_MOUSE_BUTTON_5_DOWN, RI_MOUSE_BUTTON_5_UP, RI_MOUSE_HWHEEL, RI_MOUSE_WHEEL, RegisterClassW,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, TranslateMessage, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_DESTROY, WM_HOTKEY, WM_INPUT, WNDCLASSW,
};
use windows::core::w;

use crate::protocol::{
    Frame, InputEvent, KeyCode, KeyState, Message, Modifier, MouseButton, ProtocolHello,
};
use crate::transport::FrameWriter;

const HOTKEY_ID_TOGGLE: i32 = 1;
const WHEEL_DELTA: i32 = 120;

static HOST_STATE: OnceLock<Mutex<HostState>> = OnceLock::new();

pub async fn run_host(peer: String, layout: String) -> Result<()> {
    if layout != "mac-left" {
        bail!("only --layout mac-left is implemented right now");
    }

    let stream = TcpStream::connect(&peer)
        .await
        .with_context(|| format!("connect {peer}"))?;
    let (tx, rx) = mpsc::unbounded_channel();
    HOST_STATE
        .set(Mutex::new(HostState::new(tx, layout.clone())))
        .map_err(|_| anyhow!("Windows host state was already initialized"))?;

    tokio::spawn(writer_task(stream, rx));

    info!(%peer, %layout, "starting Windows host capture");
    run_message_loop().context("run Windows host message loop")
}

async fn writer_task(stream: TcpStream, mut rx: mpsc::UnboundedReceiver<HostCommand>) {
    let mut writer = FrameWriter::new(stream);
    let session_id = Uuid::new_v4();
    let mut seq = 1_u64;

    if let Err(err) = writer
        .write_frame(Frame::new(
            session_id,
            seq,
            Message::Hello(ProtocolHello {
                protocol_version: crate::protocol::PROTOCOL_VERSION,
                role: "windows-host".to_owned(),
                device_name: std::env::var("COMPUTERNAME")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "windows".to_owned()),
            }),
        ))
        .await
    {
        error!(?err, "failed to send host hello");
        return;
    }
    seq += 1;

    while let Some(command) = rx.recv().await {
        let message = match command {
            HostCommand::Input(event) => Message::Input(event),
            HostCommand::Reset => Message::InputReset,
        };

        if let Err(err) = writer
            .write_frame(Frame::new(session_id, seq, message))
            .await
        {
            error!(?err, "host writer disconnected");
            break;
        }
        seq += 1;
    }
}

fn run_message_loop() -> Result<()> {
    let hwnd = unsafe { create_message_window()? };
    let setup = unsafe { register_input(hwnd).and_then(|_| register_hotkey(hwnd)) };

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
                release_remote_controls();
                let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID_TOGGLE);
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
        release_remote_controls();
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID_TOGGLE);
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
    unsafe {
        RegisterHotKey(
            Some(hwnd),
            HOTKEY_ID_TOGGLE,
            modifiers,
            u32::from(VK_OEM_5.0),
        )
        .context("RegisterHotKey Ctrl+Alt+\\")
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_HOTKEY if wparam.0 as i32 == HOTKEY_ID_TOGGLE => {
            if let Err(err) = toggle_remote("hotkey") {
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
                release_remote_controls();
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
    if size < size_of::<RAWINPUT>() as u32 {
        bail!("raw input packet is too small: {size}");
    }

    let mut data = vec![0_u8; size as usize];
    let read = unsafe {
        GetRawInputData(
            handle,
            RID_INPUT,
            Some(data.as_mut_ptr().cast::<c_void>()),
            &mut size,
            header_size,
        )
    };
    if read == u32::MAX {
        return Err(windows::core::Error::from_thread()).context("GetRawInputData body");
    }

    Ok(unsafe { *(data.as_ptr().cast::<RAWINPUT>()) })
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
        state.set_remote_active(true, "left edge")?;
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
    let events = state.keyboard_events(vkey, key_state);
    if state.remote_active {
        for event in events {
            state.send(HostCommand::Input(event))?;
        }
    }
    Ok(())
}

fn toggle_remote(reason: &'static str) -> Result<()> {
    let mut state = lock_state()?;
    let active = !state.remote_active;
    state.set_remote_active(active, reason)
}

fn cursor_at_left_edge() -> bool {
    unsafe {
        let mut point = Default::default();
        if GetCursorPos(&mut point).is_err() {
            return false;
        }
        point.x <= GetSystemMetrics(SM_XVIRTUALSCREEN) + 1
    }
}

unsafe fn release_remote_controls() {
    let _ = unsafe { ClipCursor(None) };
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
    Input(InputEvent),
    Reset,
}

struct HostState {
    tx: mpsc::UnboundedSender<HostCommand>,
    remote_active: bool,
    layout: String,
    pressed_modifier_keys: HashSet<u16>,
    active_modifiers: HashSet<Modifier>,
}

impl HostState {
    fn new(tx: mpsc::UnboundedSender<HostCommand>, layout: String) -> Self {
        Self {
            tx,
            remote_active: false,
            layout,
            pressed_modifier_keys: HashSet::new(),
            active_modifiers: HashSet::new(),
        }
    }

    fn set_remote_active(&mut self, active: bool, reason: &'static str) -> Result<()> {
        if self.remote_active == active {
            return Ok(());
        }

        self.remote_active = active;
        self.pressed_modifier_keys.clear();
        self.active_modifiers.clear();

        if active {
            unsafe { clip_cursor_to_left_edge()? };
            info!(reason, "remote macOS control enabled");
        } else {
            unsafe { release_remote_controls() };
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
}

unsafe fn clip_cursor_to_left_edge() -> Result<()> {
    let left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    let rect = windows::Win32::Foundation::RECT {
        left,
        top,
        right: left + 2,
        bottom: top + height,
    };
    unsafe { ClipCursor(Some(&rect)).context("ClipCursor left edge") }
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
    match vkey {
        v if v == VK_OEM_5.0 => KeyCode::Backslash,
        v if v == VK_ESCAPE.0 => KeyCode::Escape,
        v if v == VK_SPACE.0 => KeyCode::Space,
        v if v == VK_RETURN.0 => KeyCode::Enter,
        v if v == VK_TAB.0 => KeyCode::Tab,
        _ => KeyCode::Other(u32::from(vkey)),
    }
}
