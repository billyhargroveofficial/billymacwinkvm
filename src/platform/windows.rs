use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::mem::size_of;
use std::net::UdpSocket as StdUdpSocket;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio::time::{Duration, MissedTickBehavior, interval};
use tracing::{error, info, trace, warn};
use uuid::Uuid;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
};
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, HIGH_PRIORITY_CLASS, SetPriorityClass, SetThreadPriority,
    THREAD_PRIORITY_HIGHEST,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey,
    VK_BACK, VK_CAPITAL, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_INSERT,
    VK_LCONTROL, VK_LEFT, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_NEXT, VK_OEM_1, VK_OEM_2,
    VK_OEM_3, VK_OEM_4, VK_OEM_5, VK_OEM_6, VK_OEM_7, VK_OEM_102, VK_OEM_COMMA, VK_OEM_MINUS,
    VK_OEM_PERIOD, VK_OEM_PLUS, VK_PAUSE, VK_PRIOR, VK_RCONTROL, VK_RETURN, VK_RIGHT, VK_RMENU,
    VK_RSHIFT, VK_RWIN, VK_SCROLL, VK_SHIFT, VK_SNAPSHOT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::Input::{
    GetRawInputBuffer, GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RID_INPUT, RIDEV_DEVNOTIFY, RIDEV_INPUTSINK, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
    RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, ClipCursor, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetCursorPos, GetMessageW, GetSystemMetrics, HC_ACTION, HHOOK, HWND_MESSAGE, KBDLLHOOKSTRUCT,
    KillTimer, MSG, PostQuitMessage, RI_KEY_BREAK, RI_MOUSE_BUTTON_1_DOWN, RI_MOUSE_BUTTON_1_UP,
    RI_MOUSE_BUTTON_2_DOWN, RI_MOUSE_BUTTON_2_UP, RI_MOUSE_BUTTON_3_DOWN, RI_MOUSE_BUTTON_3_UP,
    RI_MOUSE_BUTTON_4_DOWN, RI_MOUSE_BUTTON_4_UP, RI_MOUSE_BUTTON_5_DOWN, RI_MOUSE_BUTTON_5_UP,
    RI_MOUSE_HWHEEL, RI_MOUSE_WHEEL, RegisterClassW, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SetCursorPos, SetTimer, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WINDOW_EX_STYLE, WINDOW_STYLE, WM_DESTROY, WM_HOTKEY, WM_INPUT,
    WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WNDCLASSW,
};
use windows::core::w;

use crate::latency;
use crate::protocol::{
    ClientControlEvent, Frame, HostStateEvent, InputEvent, KeyCode, KeyState, Message, Modifier,
    MotionDatagram, MouseButton, ProtocolHello,
};
use crate::transport::FrameWriter;

const HOTKEY_ID_TOGGLE_BACKSLASH: i32 = 1;
const HOTKEY_ID_TOGGLE_NON_US_BACKSLASH: i32 = 2;
const EDGE_TRIGGER_PX: i32 = 8;
const EDGE_REARM_PX: i32 = 64;
const HOTKEY_DEBOUNCE_MS: u64 = 250;
const MOUSE_FLUSH_INTERVAL_MS: u64 = 4;
const UDP_MOTION_SENDER_FLUSH_INTERVAL_MS: u64 = 1;
const MAX_MOTION_DRAIN_PER_TURN: usize = 64;
const MAX_MOTION_DELTA_PER_FLUSH: i32 = 512;
const MAC_ENTRY_X_RATIO_FROM_WINDOWS: f64 = 1.0;
const WINDOWS_RESTORE_EDGE_INSET_PX: i32 = 32;
const WHEEL_DELTA: i32 = 120;
const SCANCODE_BACKSLASH: u32 = 0x2b;
const SCANCODE_NON_US_BACKSLASH: u32 = 0x56;
const RAW_CADENCE_TIMER_ID: usize = 3;

static HOST_STATE: OnceLock<Mutex<HostState>> = OnceLock::new();
static DIRECT_MOTION_FAST: OnceLock<SharedUdpMotionWriter> = OnceLock::new();
static REMOTE_ACTIVE_FAST: AtomicBool = AtomicBool::new(false);
static LOCAL_MOUSE_CAPTURE_FAST: AtomicBool = AtomicBool::new(true);
static RAW_CADENCE_STATE: OnceLock<Mutex<RawCadenceState>> = OnceLock::new();

thread_local! {
    static RAW_INPUT_BUFFER: RefCell<Vec<RAWINPUT>> =
        RefCell::new(vec![RAWINPUT::default(); 1024]);
}

pub fn prepare_low_latency_process() {
    let timer_result = unsafe { timeBeginPeriod(1) };
    if timer_result == 0 {
        info!("enabled Windows 1ms timer period");
    } else {
        warn!(result = timer_result, "timeBeginPeriod(1) failed");
    }

    if let Err(err) = unsafe { SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS) } {
        warn!(?err, "failed to raise Windows process priority");
    } else {
        info!("raised Windows process priority");
    }

    prepare_low_latency_thread("main thread");
}

pub(crate) fn prepare_low_latency_thread(label: &'static str) {
    if let Err(err) = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) } {
        warn!(?err, label, "failed to raise Windows thread priority");
    } else {
        info!(label, "raised Windows thread priority");
    }
}

pub async fn run_host(
    peer: String,
    layout: String,
    activate_on_start: bool,
    entry_x_ratio: f64,
    entry_y_ratio: f64,
    no_local_capture: bool,
) -> Result<()> {
    if layout != "mac-left" {
        bail!("only --layout mac-left is implemented right now");
    }
    let entry_x_ratio = entry_x_ratio.clamp(0.02, 0.98);
    let entry_y_ratio = entry_y_ratio.clamp(0.02, 0.98);
    let local_mouse_capture = !no_local_capture;
    LOCAL_MOUSE_CAPTURE_FAST.store(local_mouse_capture, Ordering::Relaxed);

    crate::trace::init("win-host");
    crate::trace::start_freeze_detector();

    let stream =
        crate::transport::connect_tcp_with_retry(&peer, 40, Duration::from_millis(250)).await?;
    stream.set_nodelay(true).context("set TCP_NODELAY")?;
    let motion_transport = MotionTransport::connect(&peer).await;
    let direct_motion = motion_transport.direct_writer();
    if let Some(writer) = direct_motion.clone() {
        let _ = DIRECT_MOTION_FAST.set(writer);
    }
    let (tx, rx) = mpsc::unbounded_channel();
    HOST_STATE
        .set(Mutex::new(HostState::new(
            tx,
            layout.clone(),
            direct_motion,
            activate_on_start.then_some((entry_x_ratio, entry_y_ratio)),
            local_mouse_capture,
        )))
        .map_err(|_| anyhow!("Windows host state was already initialized"))?;

    let (read_half, write_half) = stream.into_split();
    tokio::spawn(writer_task(write_half, rx, motion_transport));
    tokio::spawn(control_reader_task(read_half));

    info!(%peer, %layout, activate_on_start, entry_x_ratio, entry_y_ratio, local_mouse_capture, "starting Windows host capture");
    tokio::task::spawn_blocking(run_message_loop)
        .await
        .context("join Windows host message loop")?
        .context("run Windows host message loop")
}

pub fn run_raw_cadence(
    seconds: u32,
    install_mouse_hook: bool,
    suppress_mouse: bool,
    mode_name: &'static str,
) -> Result<()> {
    let seconds = seconds.clamp(1, 600);
    prepare_low_latency_thread("Raw Input cadence diagnostic");

    RAW_CADENCE_STATE
        .set(Mutex::new(RawCadenceState::new(
            seconds,
            mode_name,
            install_mouse_hook,
            suppress_mouse,
        )))
        .map_err(|_| anyhow!("raw cadence diagnostic can only run once per process"))?;

    let hwnd = unsafe { create_raw_cadence_window()? };
    let setup = unsafe { register_raw_cadence_input(hwnd) };
    if let Err(err) = setup {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        return Err(err);
    }

    let hook = if install_mouse_hook {
        Some(
            unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(raw_cadence_mouse_hook_proc), None, 0) }
                .context("SetWindowsHookExW WH_MOUSE_LL raw cadence")?,
        )
    } else {
        None
    };

    let timer_ms = seconds.saturating_mul(1000);
    let timer = unsafe { SetTimer(Some(hwnd), RAW_CADENCE_TIMER_ID, timer_ms, None) };
    if timer == 0 {
        if let Some(hook) = hook {
            uninstall_hook(hook.0 as isize, "WH_MOUSE_LL raw cadence");
        }
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        bail!("SetTimer failed for raw cadence diagnostic");
    }

    println!("softkvm win-raw-cadence");
    println!("mode={mode_name} seconds={seconds}");
    println!("Move the real Windows mouse continuously until the summary prints.");
    if suppress_mouse {
        println!("Mouse events are suppressed for this timed run; keyboard remains live.");
    }

    let mut msg = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if result.0 == -1 {
            break;
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
        let _ = KillTimer(Some(hwnd), RAW_CADENCE_TIMER_ID);
    }
    if let Some(hook) = hook {
        uninstall_hook(hook.0 as isize, "WH_MOUSE_LL raw cadence");
    }
    unsafe {
        let _ = DestroyWindow(hwnd);
    }

    let state = RAW_CADENCE_STATE
        .get()
        .ok_or_else(|| anyhow!("raw cadence state disappeared"))?
        .lock()
        .map_err(|_| anyhow!("raw cadence state lock poisoned"))?;
    state.print_summary();
    Ok(())
}

struct RawCadenceState {
    started: Instant,
    seconds: u32,
    mode_name: &'static str,
    install_mouse_hook: bool,
    suppress_mouse: bool,
    total_raw_messages: u64,
    mouse_events: u64,
    zero_mouse_events: u64,
    keyboard_events: u64,
    hook_events: u64,
    hook_suppressed: u64,
    devices: HashMap<usize, RawCadenceDeviceStats>,
    read_ms: Vec<f64>,
    handler_ms: Vec<f64>,
    hook_ms: Vec<f64>,
}

impl RawCadenceState {
    fn new(
        seconds: u32,
        mode_name: &'static str,
        install_mouse_hook: bool,
        suppress_mouse: bool,
    ) -> Self {
        let expected_samples = seconds.saturating_mul(2000).max(2048) as usize;
        Self {
            started: Instant::now(),
            seconds,
            mode_name,
            install_mouse_hook,
            suppress_mouse,
            total_raw_messages: 0,
            mouse_events: 0,
            zero_mouse_events: 0,
            keyboard_events: 0,
            hook_events: 0,
            hook_suppressed: 0,
            devices: HashMap::new(),
            read_ms: Vec::with_capacity(expected_samples),
            handler_ms: Vec::with_capacity(expected_samples),
            hook_ms: Vec::with_capacity(expected_samples),
        }
    }

    fn record_raw(&mut self, raw: RAWINPUT, message_started: Instant, read_elapsed: Duration) {
        self.total_raw_messages += 1;
        self.read_ms.push(latency::ms(read_elapsed));

        match raw.header.dwType {
            t if t == RIM_TYPEMOUSE.0 => {
                let mouse = unsafe { raw.data.mouse };
                let buttons = unsafe { mouse.Anonymous.Anonymous };
                let flags = u32::from(buttons.usButtonFlags);
                let dx = mouse.lLastX;
                let dy = mouse.lLastY;
                let nonzero = dx != 0 || dy != 0 || flags != 0;
                if nonzero {
                    self.mouse_events += 1;
                    let device = raw.header.hDevice.0 as usize;
                    self.devices
                        .entry(device)
                        .or_default()
                        .record(message_started, dx, dy, flags);
                } else {
                    self.zero_mouse_events += 1;
                }
            }
            t if t == RIM_TYPEKEYBOARD.0 => {
                self.keyboard_events += 1;
            }
            _ => {}
        }
    }

    fn record_handler_elapsed(&mut self, elapsed: Duration) {
        self.handler_ms.push(latency::ms(elapsed));
    }

    fn record_mouse_hook(&mut self, elapsed: Duration, suppressed: bool) {
        self.hook_events += 1;
        if suppressed {
            self.hook_suppressed += 1;
        }
        self.hook_ms.push(latency::ms(elapsed));
    }

    fn print_summary(&self) {
        println!();
        println!("== softkvm win-raw-cadence summary ==");
        println!(
            "mode={} seconds={} elapsed_ms={:.3}",
            self.mode_name,
            self.seconds,
            latency::ms(self.started.elapsed())
        );
        println!(
            "hooks install={} suppress_mouse={} hook_events={} hook_suppressed={}",
            self.install_mouse_hook, self.suppress_mouse, self.hook_events, self.hook_suppressed
        );
        println!(
            "raw_messages={} mouse_events={} zero_mouse_events={} keyboard_events={}",
            self.total_raw_messages,
            self.mouse_events,
            self.zero_mouse_events,
            self.keyboard_events
        );
        print_histogram("getrawinput_ms", &self.read_ms);
        print_histogram("raw_handler_ms", &self.handler_ms);
        print_histogram("mouse_hook_ms", &self.hook_ms);

        if self.devices.is_empty() {
            println!("device none: no nonzero mouse events captured");
            return;
        }

        for (device, stats) in &self.devices {
            println!(
                "device=0x{device:x} events={} dx_total={} dy_total={} buttons_or_wheel_events={}",
                stats.events, stats.dx_total, stats.dy_total, stats.button_or_wheel_events
            );
            print_histogram("  raw_gap_ms", &stats.gap_ms);
        }
    }
}

#[derive(Default)]
struct RawCadenceDeviceStats {
    events: u64,
    last_event: Option<Instant>,
    gap_ms: Vec<f64>,
    dx_total: i64,
    dy_total: i64,
    button_or_wheel_events: u64,
}

impl RawCadenceDeviceStats {
    fn record(&mut self, now: Instant, dx: i32, dy: i32, flags: u32) {
        if let Some(last) = self.last_event.replace(now) {
            self.gap_ms.push(latency::ms(now.duration_since(last)));
        }
        if self.last_event.is_none() {
            self.last_event = Some(now);
        }
        self.events += 1;
        self.dx_total += i64::from(dx);
        self.dy_total += i64::from(dy);
        if flags != 0 {
            self.button_or_wheel_events += 1;
        }
    }
}

fn print_histogram(label: &str, samples: &[f64]) {
    if samples.is_empty() {
        println!("{label}: count=0");
        return;
    }

    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let sum: f64 = sorted.iter().sum();
    let mean = sum / sorted.len() as f64;
    println!(
        "{label}: count={} mean={:.3} p50={:.3} p95={:.3} p99={:.3} p99.9={:.3} max={:.3}",
        sorted.len(),
        mean,
        percentile(&sorted, 0.50),
        percentile(&sorted, 0.95),
        percentile(&sorted, 0.99),
        percentile(&sorted, 0.999),
        sorted.last().copied().unwrap_or(0.0),
    );
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * q.clamp(0.0, 1.0)).round() as usize;
    sorted[index]
}

unsafe fn create_raw_cadence_window() -> Result<HWND> {
    let module = unsafe { GetModuleHandleW(None).context("GetModuleHandleW")? };
    let hinstance = HINSTANCE(module.0);
    let class_name = w!("SoftKvmRawCadenceWindow");
    let class = WNDCLASSW {
        hInstance: hinstance,
        lpszClassName: class_name,
        lpfnWndProc: Some(raw_cadence_wnd_proc),
        ..Default::default()
    };

    let atom = unsafe { RegisterClassW(&class) };
    if atom == 0 {
        return Err(windows::core::Error::from_thread()).context("RegisterClassW raw cadence");
    }

    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("softkvm raw cadence"),
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
    .context("CreateWindowExW raw cadence")
}

unsafe fn register_raw_cadence_input(hwnd: HWND) -> Result<()> {
    let devices = [RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: 0x02,
        dwFlags: RIDEV_INPUTSINK | RIDEV_DEVNOTIFY,
        hwndTarget: hwnd,
    }];

    unsafe {
        RegisterRawInputDevices(&devices, size_of::<RAWINPUTDEVICE>() as u32)
            .context("RegisterRawInputDevices raw cadence")
    }
}

unsafe extern "system" fn raw_cadence_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_INPUT => {
            if let Err(err) = handle_raw_cadence_input(lparam) {
                warn!(?err, "raw cadence input failed");
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_TIMER if wparam.0 == RAW_CADENCE_TIMER_ID => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn handle_raw_cadence_input(lparam: LPARAM) -> Result<()> {
    let started = Instant::now();
    let raw = unsafe { read_raw_input(lparam)? };
    let read_elapsed = started.elapsed();

    let mut state = RAW_CADENCE_STATE
        .get()
        .ok_or_else(|| anyhow!("raw cadence state is not initialized"))?
        .lock()
        .map_err(|_| anyhow!("raw cadence state lock poisoned"))?;
    state.record_raw(raw, started, read_elapsed);
    state.record_handler_elapsed(started.elapsed());
    Ok(())
}

unsafe extern "system" fn raw_cadence_mouse_hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let started = Instant::now();
    let mut suppress = false;

    if code == HC_ACTION as i32
        && let Some(state) = RAW_CADENCE_STATE.get()
        && let Ok(mut state) = state.lock()
    {
        suppress = state.suppress_mouse;
        state.record_mouse_hook(started.elapsed(), suppress);
    }

    if suppress {
        return LRESULT(1);
    }

    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

enum MotionTransport {
    Udp { writer: SharedUdpMotionWriter },
    Tcp,
}

impl MotionTransport {
    async fn connect(peer: &str) -> Self {
        let configured = std::env::var("SOFTKVM_MOTION_TRANSPORT")
            .unwrap_or_else(|_| "udp".to_owned())
            .to_ascii_lowercase();
        if !matches!(configured.as_str(), "udp" | "udp-binary" | "binary") {
            info!(mode = configured, "using tcp/json motion transport");
            return Self::Tcp;
        }

        match crate::transport::bind_udp_with_fallback() {
            Ok(socket) => match socket.connect(peer) {
                Ok(()) => {
                    info!(%peer, "using udp/binary motion transport");
                    Self::Udp {
                        writer: SharedUdpMotionWriter::new(socket),
                    }
                }
                Err(err) => {
                    warn!(?err, %peer, "udp motion connect failed; falling back to tcp/json motion");
                    Self::Tcp
                }
            },
            Err(err) => {
                warn!(
                    ?err,
                    "udp motion bind failed; falling back to tcp/json motion"
                );
                Self::Tcp
            }
        }
    }

    async fn send_motion(&mut self, dx: i32, dy: i32) -> Result<bool> {
        match self {
            Self::Udp { writer } if writer.confirmed() => {
                writer.send_motion_if_confirmed(dx, dy, "writer")
            }
            Self::Udp { writer } => {
                writer.send_motion(0, 0, "probe")?;
                Ok(false)
            }
            Self::Tcp => Ok(false),
        }
    }

    async fn probe(&mut self) {
        if let Self::Udp { writer } = self
            && !writer.confirmed()
            && let Err(err) = writer.send_motion(0, 0, "probe")
        {
            warn!(
                ?err,
                "udp motion probe failed; falling back to tcp/json motion"
            );
            *self = Self::Tcp;
        }
    }

    fn confirm(&mut self) {
        if let Self::Udp { writer } = self
            && writer.confirm()
        {
            info!("udp motion transport confirmed by macOS client");
        }
    }

    fn direct_writer(&self) -> Option<SharedUdpMotionWriter> {
        match self {
            Self::Udp { writer } => Some(writer.clone()),
            Self::Tcp => None,
        }
    }
}

#[derive(Clone)]
struct SharedUdpMotionWriter {
    socket: Arc<StdUdpSocket>,
    seq: Arc<AtomicU64>,
    confirmed: Arc<AtomicBool>,
    pending_dx: Arc<AtomicI32>,
    pending_dy: Arc<AtomicI32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UdpSendMode {
    Immediate,
    Coalesced,
}

impl SharedUdpMotionWriter {
    fn new(socket: StdUdpSocket) -> Self {
        let writer = Self {
            socket: Arc::new(socket),
            seq: Arc::new(AtomicU64::new(1)),
            confirmed: Arc::new(AtomicBool::new(true)),
            pending_dx: Arc::new(AtomicI32::new(0)),
            pending_dy: Arc::new(AtomicI32::new(0)),
        };
        if udp_send_mode() == UdpSendMode::Coalesced {
            info!("using coalesced UDP motion sender");
            writer.spawn_motion_sender();
        } else {
            info!("using immediate UDP motion sends");
        }
        writer
    }

    fn confirmed(&self) -> bool {
        self.confirmed.load(Ordering::Acquire)
    }

    fn confirm(&self) -> bool {
        !self.confirmed.swap(true, Ordering::AcqRel)
    }

    fn send_motion_if_confirmed(&self, dx: i32, dy: i32, path: &'static str) -> Result<bool> {
        if !self.confirmed() {
            return Ok(false);
        }
        if matches!(path, "direct" | "writer" | "raw-fast") {
            match udp_send_mode() {
                UdpSendMode::Immediate => self.send_motion_immediate(dx, dy, "direct-immediate")?,
                UdpSendMode::Coalesced => self.queue_motion(dx, dy),
            }
            return Ok(true);
        }
        self.send_motion(dx, dy, path)?;
        Ok(true)
    }

    fn queue_motion(&self, dx: i32, dy: i32) {
        accumulate_i32(&self.pending_dx, dx);
        accumulate_i32(&self.pending_dy, dy);
    }

    fn send_motion(&self, dx: i32, dy: i32, path: &'static str) -> Result<()> {
        self.send_motion_immediate(dx, dy, path)
    }

    fn send_motion_immediate(&self, dx: i32, dy: i32, path: &'static str) -> Result<()> {
        let packet = MotionDatagram {
            seq: self.seq.fetch_add(1, Ordering::Relaxed),
            dx: dx.clamp(-MAX_MOTION_DELTA_PER_FLUSH, MAX_MOTION_DELTA_PER_FLUSH),
            dy: dy.clamp(-MAX_MOTION_DELTA_PER_FLUSH, MAX_MOTION_DELTA_PER_FLUSH),
            t_send_us: crate::trace::now_us() as u32,
        };
        // SKM2 (with the sender stamp) only while tracing, so untraced runs
        // keep the exact SKM1 wire format older clients expect.
        let encoded_v2;
        let encoded_v1;
        let encoded: &[u8] = if crate::trace::enabled() {
            encoded_v2 = packet.encode_v2();
            &encoded_v2
        } else {
            encoded_v1 = packet.encode();
            &encoded_v1
        };
        let started = Instant::now();
        crate::trace::stamp(
            crate::trace::Stage::WinUdpPre,
            packet.seq,
            packet.dx,
            packet.dy,
            0,
        );
        let sent = self
            .socket
            .send(encoded)
            .context("send udp motion packet")?;
        crate::trace::stamp(
            crate::trace::Stage::WinUdpPost,
            packet.seq,
            packet.dx,
            packet.dy,
            0,
        );
        if sent != encoded.len() {
            bail!("short udp motion send: {sent}/{}", encoded.len());
        }
        let elapsed = started.elapsed();
        if latency::report(elapsed) {
            info!(
                target: "softkvm::latency",
                path,
                seq = packet.seq,
                dx = packet.dx,
                dy = packet.dy,
                elapsed_ms = latency::ms(elapsed),
                "windows udp motion send latency"
            );
        }
        Ok(())
    }

    fn spawn_motion_sender(&self) {
        let writer = self.clone();
        thread::Builder::new()
            .name("softkvm-udp-motion-sender".to_owned())
            .spawn(move || {
                prepare_low_latency_thread("UDP motion sender");
                loop {
                    thread::sleep(Duration::from_millis(UDP_MOTION_SENDER_FLUSH_INTERVAL_MS));
                    if !writer.confirmed() {
                        continue;
                    }

                    let dx = writer.pending_dx.swap(0, Ordering::AcqRel);
                    let dy = writer.pending_dy.swap(0, Ordering::AcqRel);
                    if dx == 0 && dy == 0 {
                        continue;
                    }

                    if let Err(err) = writer.send_motion_immediate(dx, dy, "sender-thread") {
                        warn!(?err, "udp motion sender thread failed to flush motion");
                    }
                }
            })
            .expect("spawn UDP motion sender thread");
    }
}

fn udp_send_mode() -> UdpSendMode {
    static MODE: OnceLock<UdpSendMode> = OnceLock::new();
    *MODE.get_or_init(|| {
        match std::env::var("SOFTKVM_UDP_SEND_MODE")
            .unwrap_or_else(|_| "immediate".to_owned())
            .to_ascii_lowercase()
            .as_str()
        {
            "immediate" | "direct" => UdpSendMode::Immediate,
            "coalesced" | "batch" | "sender-thread" => UdpSendMode::Coalesced,
            other => {
                warn!(
                    value = other,
                    "unknown SOFTKVM_UDP_SEND_MODE; using immediate"
                );
                UdpSendMode::Immediate
            }
        }
    })
}

fn accumulate_i32(cell: &AtomicI32, delta: i32) {
    let _ = cell.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        Some(
            current
                .saturating_add(delta)
                .clamp(-MAX_MOTION_DELTA_PER_FLUSH, MAX_MOTION_DELTA_PER_FLUSH),
        )
    });
}

async fn writer_task(
    stream: OwnedWriteHalf,
    mut rx: mpsc::UnboundedReceiver<HostCommand>,
    mut motion_transport: MotionTransport,
) {
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
    let mut deferred_command = None;
    let mut flush_timer = interval(Duration::from_millis(MOUSE_FLUSH_INTERVAL_MS));
    flush_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        if let Some(command) = deferred_command.take() {
            if !handle_host_command(
                command,
                &mut rx,
                &mut writer,
                &mut motion_transport,
                session_id,
                &mut seq,
                &mut pending_dx,
                &mut pending_dy,
                &mut deferred_command,
            )
            .await
            {
                break;
            }
            continue;
        }

        tokio::select! {
            biased;

            _ = flush_timer.tick(), if pending_dx != 0 || pending_dy != 0 => {
                if !flush_pending_motion(&mut writer, session_id, &mut seq, &mut pending_dx, &mut pending_dy).await {
                    break;
                }
            }
            command = rx.recv() => {
                let Some(command) = command else {
                    if !flush_pending_motion(&mut writer, session_id, &mut seq, &mut pending_dx, &mut pending_dy).await {
                        break;
                    }
                    break;
                };

                if !handle_host_command(
                    command,
                    &mut rx,
                    &mut writer,
                    &mut motion_transport,
                    session_id,
                    &mut seq,
                    &mut pending_dx,
                    &mut pending_dy,
                    &mut deferred_command,
                ).await {
                    break;
                }
            }
        }
    }
}

async fn handle_host_command(
    command: HostCommand,
    rx: &mut mpsc::UnboundedReceiver<HostCommand>,
    writer: &mut FrameWriter<OwnedWriteHalf>,
    motion_transport: &mut MotionTransport,
    session_id: Uuid,
    seq: &mut u64,
    pending_dx: &mut i32,
    pending_dy: &mut i32,
    deferred_command: &mut Option<HostCommand>,
) -> bool {
    match command {
        HostCommand::Input(InputEvent::MouseMotion { dx, dy }) => {
            match motion_transport.send_motion(dx, dy).await {
                Ok(true) => return true,
                Ok(false) => {}
                Err(err) => {
                    warn!(
                        ?err,
                        "udp motion send failed; falling back to tcp/json motion"
                    );
                    *motion_transport = MotionTransport::Tcp;
                }
            }

            *pending_dx = (*pending_dx).saturating_add(dx);
            *pending_dy = (*pending_dy).saturating_add(dy);

            for _ in 0..MAX_MOTION_DRAIN_PER_TURN {
                let Ok(command) = rx.try_recv() else {
                    break;
                };
                match command {
                    HostCommand::Input(InputEvent::MouseMotion { dx, dy }) => {
                        *pending_dx = (*pending_dx).saturating_add(dx);
                        *pending_dy = (*pending_dy).saturating_add(dy);
                    }
                    other => {
                        *deferred_command = Some(other);
                        break;
                    }
                }
            }
            true
        }
        HostCommand::InputImmediate(event) => {
            if !flush_pending_motion(writer, session_id, seq, pending_dx, pending_dy).await {
                return false;
            }
            write_host_message(
                writer,
                session_id,
                seq,
                Message::Input(event),
                "host writer disconnected",
            )
            .await
        }
        HostCommand::HostState { active, reason } => {
            if !flush_pending_motion(writer, session_id, seq, pending_dx, pending_dy).await {
                return false;
            }
            let ok = write_host_message(
                writer,
                session_id,
                seq,
                Message::HostState(HostStateEvent {
                    remote_active: active,
                    reason: reason.to_owned(),
                    entry_x_ratio: None,
                    entry_y_ratio: None,
                }),
                "host writer disconnected",
            )
            .await;
            if ok && active {
                motion_transport.probe().await;
            }
            ok
        }
        HostCommand::HostStateWithEntry {
            active,
            reason,
            entry_x_ratio,
            entry_y_ratio,
        } => {
            if !flush_pending_motion(writer, session_id, seq, pending_dx, pending_dy).await {
                return false;
            }
            let ok = write_host_message(
                writer,
                session_id,
                seq,
                Message::HostState(HostStateEvent {
                    remote_active: active,
                    reason: reason.to_owned(),
                    entry_x_ratio: Some(entry_x_ratio),
                    entry_y_ratio: Some(entry_y_ratio),
                }),
                "host writer disconnected",
            )
            .await;
            if ok && active {
                motion_transport.probe().await;
            }
            ok
        }
        HostCommand::UdpMotionReady => {
            motion_transport.confirm();
            true
        }
        other => {
            if !flush_pending_motion(writer, session_id, seq, pending_dx, pending_dy).await {
                return false;
            }
            write_host_message(
                writer,
                session_id,
                seq,
                message_from_host_command(other),
                "host writer disconnected",
            )
            .await
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

    let raw_dx = *pending_dx;
    let raw_dy = *pending_dy;
    let dx =
        std::mem::take(pending_dx).clamp(-MAX_MOTION_DELTA_PER_FLUSH, MAX_MOTION_DELTA_PER_FLUSH);
    let dy =
        std::mem::take(pending_dy).clamp(-MAX_MOTION_DELTA_PER_FLUSH, MAX_MOTION_DELTA_PER_FLUSH);
    if latency::enabled() || dx != raw_dx || dy != raw_dy {
        info!(
            target: "softkvm::latency",
            raw_dx,
            raw_dy,
            sent_dx = dx,
            sent_dy = dy,
            dropped_dx = raw_dx - dx,
            dropped_dy = raw_dy - dy,
            "windows motion flush batch"
        );
    }
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
    let message_label = message.label();
    let current_seq = *seq;
    let started = Instant::now();
    if let Err(err) = writer
        .write_frame(Frame::new(session_id, current_seq, message))
        .await
    {
        error!(?err, "host writer disconnected");
        release_host_state_after_transport_loss(disconnect_reason);
        return false;
    }
    let elapsed = started.elapsed();
    if latency::report(elapsed) {
        info!(
            target: "softkvm::latency",
            seq = current_seq,
            message = message_label,
            elapsed_ms = latency::ms(elapsed),
            "windows tcp write latency"
        );
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
        HostCommand::Input(event) | HostCommand::InputImmediate(event) => Message::Input(event),
        HostCommand::Reset => Message::InputReset,
        HostCommand::UdpMotionReady => unreachable!("handled before serialization"),
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

        if let Message::ClientControl(control) = frame.message {
            match control {
                ClientControlEvent::ReleaseHost {
                    reason,
                    entry_x_ratio,
                    entry_y_ratio,
                } => {
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
                ClientControlEvent::UdpMotionReady => match lock_state() {
                    Ok(state) => {
                        if let Err(err) = state.send(HostCommand::UdpMotionReady) {
                            warn!(?err, "failed to forward udp motion ready control");
                        }
                    }
                    Err(err) => warn!(?err, "failed to lock host state for udp motion ready"),
                },
            }
        }
    }
}

fn run_message_loop() -> Result<()> {
    prepare_low_latency_thread("Raw Input message loop");

    let hwnd = unsafe { create_message_window()? };
    let setup = unsafe { register_input(hwnd).and_then(|_| register_hotkey(hwnd)) }
        .and_then(|_| install_persistent_hooks());

    if let Err(err) = setup {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        return Err(err);
    }

    info!("Windows Raw Input host is ready; Ctrl+Alt+\\ toggles remote control");
    if let Ok(mut state) = lock_state() {
        if let Some((entry_x_ratio, entry_y_ratio)) = state.activate_on_start.take()
            && let Err(err) = state.set_remote_active(
                true,
                "activate-on-start",
                Some(entry_x_ratio),
                Some(entry_y_ratio),
            )
        {
            warn!(?err, "failed to activate remote control on start");
        }
    }

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
            let result = if buffered_raw_input_enabled() {
                handle_raw_input_buffered(lparam)
            } else {
                handle_raw_input(lparam)
            };
            if let Err(err) = result {
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
    handle_raw_input_struct(raw)
}

fn handle_raw_input_struct(raw: RAWINPUT) -> Result<()> {
    match raw.header.dwType {
        t if t == RIM_TYPEMOUSE.0 => handle_mouse(raw),
        t if t == RIM_TYPEKEYBOARD.0 => handle_keyboard(raw),
        _ => Ok(()),
    }
}

fn handle_raw_input_buffered(lparam: LPARAM) -> Result<()> {
    let started = Instant::now();
    handle_raw_input(lparam)?;
    let drained = RAW_INPUT_BUFFER.with(|cell| drain_raw_input_buffer(&mut cell.borrow_mut()))?;
    let total = 1 + drained;

    let elapsed = started.elapsed();
    if latency::report(elapsed) {
        info!(
            target: "softkvm::latency",
            packets = total,
            elapsed_ms = latency::ms(elapsed),
            "windows buffered raw input drain"
        );
    }

    Ok(())
}

fn drain_raw_input_buffer(buffer: &mut Vec<RAWINPUT>) -> Result<usize> {
    let mut total = 0_usize;

    loop {
        let mut buffer_size = (buffer.len() * size_of::<RAWINPUT>()) as u32;
        let count = unsafe {
            GetRawInputBuffer(
                Some(buffer.as_mut_ptr()),
                &mut buffer_size,
                size_of::<RAWINPUTHEADER>() as u32,
            )
        };

        if count == u32::MAX {
            return Err(windows::core::Error::from_thread()).context("GetRawInputBuffer");
        }

        if count == 0 {
            break;
        }

        unsafe {
            handle_raw_input_buffer_entries(buffer.as_ptr(), count)?;
        }
        total += count as usize;

        if count < buffer.len() as u32 {
            break;
        }
    }

    Ok(total)
}

unsafe fn handle_raw_input_buffer_entries(base: *const RAWINPUT, count: u32) -> Result<()> {
    let mut current = base;
    for _ in 0..count {
        let raw_ref = unsafe { &*current };
        let step = raw_ref
            .header
            .dwSize
            .max(size_of::<RAWINPUTHEADER>() as u32) as usize;
        let raw = unsafe { std::ptr::read_unaligned(current) };
        handle_raw_input_struct(raw)?;
        current = unsafe { current.cast::<u8>().add(step).cast::<RAWINPUT>() };
    }
    Ok(())
}

fn buffered_raw_input_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        !std::env::var("SOFTKVM_RAW_INPUT_READER")
            .is_ok_and(|value| value.eq_ignore_ascii_case("lparam"))
    })
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

fn fast_direct_motion_writer() -> Option<&'static SharedUdpMotionWriter> {
    DIRECT_MOTION_FAST.get()
}

/// Keep the lock-free flag and the trace session marker in step.
fn set_remote_active_fast(active: bool) {
    REMOTE_ACTIVE_FAST.store(active, Ordering::Relaxed);
    crate::trace::set_active(active);
}

static RAW_MOUSE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn handle_mouse(raw: RAWINPUT) -> Result<()> {
    let started = Instant::now();
    let mouse = unsafe { raw.data.mouse };
    let buttons = unsafe { mouse.Anonymous.Anonymous };
    let flags = u32::from(buttons.usButtonFlags);
    let dx = mouse.lLastX;
    let dy = mouse.lLastY;
    if dx != 0 || dy != 0 || flags != 0 {
        crate::trace::stamp(
            crate::trace::Stage::WinRawIn,
            RAW_MOUSE_COUNTER.fetch_add(1, Ordering::Relaxed),
            dx,
            dy,
            flags,
        );
    }

    if REMOTE_ACTIVE_FAST.load(Ordering::Relaxed) && flags == 0 && (dx != 0 || dy != 0) {
        if let Some(writer) = fast_direct_motion_writer() {
            match writer.send_motion_if_confirmed(dx, dy, "raw-fast") {
                Ok(true) => {
                    let elapsed = started.elapsed();
                    if latency::report(elapsed) {
                        info!(
                            target: "softkvm::latency",
                            remote_active = true,
                            event_count = 1,
                            elapsed_ms = latency::ms(elapsed),
                            "windows mouse raw input handling"
                        );
                    }
                    return Ok(());
                }
                Ok(false) => {}
                Err(err) => {
                    warn!(
                        ?err,
                        "fast direct udp motion send failed; falling back to host state"
                    );
                }
            }
        }
    }

    let mut events = Vec::new();

    if dx != 0 || dy != 0 {
        events.push(InputEvent::MouseMotion { dx, dy });
    }

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
    if !state.remote_active && state.layout == "mac-left" {
        state.update_left_edge_arm();
    }

    let mut activated_this_packet = false;
    if !state.remote_active
        && state.layout == "mac-left"
        && state.left_edge_armed
        && dx < 0
        && cursor_at_left_edge()
    {
        let (_, y_ratio) =
            current_monitor_cursor_ratios().unwrap_or((MAC_ENTRY_X_RATIO_FROM_WINDOWS, 0.5));
        state.set_remote_active(
            true,
            "left edge",
            Some(MAC_ENTRY_X_RATIO_FROM_WINDOWS),
            Some(y_ratio),
        )?;
        activated_this_packet = true;
        let elapsed = started.elapsed();
        info!(
            target: "softkvm::latency",
            dx,
            dy,
            y_ratio,
            elapsed_ms = latency::ms(elapsed),
            "windows left-edge activation packet"
        );
    }

    let event_count = events.len();
    let has_discrete = events
        .iter()
        .any(|event| !matches!(event, InputEvent::MouseMotion { .. }));
    if state.remote_active {
        for event in events {
            if activated_this_packet && let InputEvent::MouseMotion { dx, dy } = event {
                state.send(HostCommand::InputImmediate(clamp_activation_motion(dx, dy)))?;
                continue;
            }
            if let InputEvent::MouseMotion { dx, dy } = event {
                if has_discrete {
                    state.send(HostCommand::InputImmediate(InputEvent::MouseMotion {
                        dx,
                        dy,
                    }))?;
                    continue;
                }
                match state.send_direct_motion(dx, dy) {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(err) => {
                        warn!(
                            ?err,
                            "direct udp motion send failed; falling back to host writer"
                        );
                    }
                }
            }
            state.send(HostCommand::Input(event))?;
        }
    }

    let elapsed = started.elapsed();
    if latency::report(elapsed) {
        info!(
            target: "softkvm::latency",
            remote_active = state.remote_active,
            event_count,
            elapsed_ms = latency::ms(elapsed),
            "windows mouse raw input handling"
        );
    }
    Ok(())
}

fn clamp_activation_motion(dx: i32, dy: i32) -> InputEvent {
    InputEvent::MouseMotion {
        dx: dx.clamp(-64, 0),
        dy: dy.clamp(-64, 64),
    }
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

fn clamp_restore_point_inside_left_edge(rect: RECT, mut point: POINT) -> POINT {
    let min_x = (rect.left + EDGE_TRIGGER_PX + WINDOWS_RESTORE_EDGE_INSET_PX).min(rect.right - 1);
    let max_x = rect.right - 1;
    if max_x >= rect.left {
        point.x = point.x.clamp(rect.left, max_x).max(min_x);
    }
    let max_y = (rect.bottom - 1).max(rect.top);
    point.y = point.y.clamp(rect.top, max_y);
    point
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
        set_remote_active_fast(false);
        LOCAL_MOUSE_CAPTURE_FAST.store(true, Ordering::Relaxed);
        state.left_edge_armed = false;
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
            set_remote_active_fast(false);
            LOCAL_MOUSE_CAPTURE_FAST.store(true, Ordering::Relaxed);
            state.left_edge_armed = false;
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

fn install_persistent_hooks() -> Result<()> {
    let mut state = lock_state()?;
    state.install_mouse_hook()?;
    state.install_keyboard_hook()?;
    info!("persistent Windows input hooks are ready for remote suppression and Ctrl+(Alt|Win)+\\");
    Ok(())
}

unsafe extern "system" fn mouse_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32
        && REMOTE_ACTIVE_FAST.load(Ordering::Relaxed)
        && LOCAL_MOUSE_CAPTURE_FAST.load(Ordering::Relaxed)
    {
        crate::trace::stamp(crate::trace::Stage::WinHook, 0, 0, 0, wparam.0 as u32);
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
                    trace!(
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
        trace!(
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
    InputImmediate(InputEvent),
    UdpMotionReady,
    Reset,
}

struct HostState {
    tx: mpsc::UnboundedSender<HostCommand>,
    direct_motion: Option<SharedUdpMotionWriter>,
    remote_active: bool,
    layout: String,
    pressed_modifier_keys: HashSet<u16>,
    active_modifiers: HashSet<Modifier>,
    mouse_hook: Option<isize>,
    keyboard_hook: Option<isize>,
    saved_cursor_pos: Option<POINT>,
    saved_monitor_rect: Option<RECT>,
    last_hotkey_toggle: Option<Instant>,
    left_edge_armed: bool,
    activate_on_start: Option<(f64, f64)>,
    local_mouse_capture: bool,
}

impl HostState {
    fn new(
        tx: mpsc::UnboundedSender<HostCommand>,
        layout: String,
        direct_motion: Option<SharedUdpMotionWriter>,
        activate_on_start: Option<(f64, f64)>,
        local_mouse_capture: bool,
    ) -> Self {
        Self {
            tx,
            direct_motion,
            remote_active: false,
            layout,
            pressed_modifier_keys: HashSet::new(),
            active_modifiers: HashSet::new(),
            mouse_hook: None,
            keyboard_hook: None,
            saved_cursor_pos: None,
            saved_monitor_rect: None,
            last_hotkey_toggle: None,
            left_edge_armed: true,
            activate_on_start,
            local_mouse_capture,
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
        let total_started = Instant::now();

        self.pressed_modifier_keys.clear();
        self.active_modifiers.clear();

        if active {
            let capture_started = Instant::now();
            if let Err(err) = self.capture_local_controls() {
                self.remote_active = false;
                set_remote_active_fast(false);
                LOCAL_MOUSE_CAPTURE_FAST.store(true, Ordering::Relaxed);
                self.release_local_controls(false, None, None);
                self.left_edge_armed = false;
                return Err(err);
            }
            let capture_elapsed = capture_started.elapsed();
            self.remote_active = true;
            set_remote_active_fast(true);
            LOCAL_MOUSE_CAPTURE_FAST.store(self.local_mouse_capture, Ordering::Relaxed);
            self.left_edge_armed = false;
            let send_state_started = Instant::now();
            let send_result = match entry_x_ratio.zip(entry_y_ratio) {
                Some((entry_x_ratio, entry_y_ratio)) => {
                    self.send(HostCommand::HostStateWithEntry {
                        active,
                        reason,
                        entry_x_ratio,
                        entry_y_ratio,
                    })
                }
                None => self.send(HostCommand::HostState { active, reason }),
            };
            if let Err(err) = send_result {
                self.remote_active = false;
                set_remote_active_fast(false);
                LOCAL_MOUSE_CAPTURE_FAST.store(true, Ordering::Relaxed);
                self.release_local_controls(true, None, None);
                return Err(err);
            }
            let send_state_elapsed = send_state_started.elapsed();
            let modifiers_started = Instant::now();
            if let Err(err) = self.send_current_modifier_state() {
                self.remote_active = false;
                set_remote_active_fast(false);
                LOCAL_MOUSE_CAPTURE_FAST.store(true, Ordering::Relaxed);
                self.release_local_controls(true, None, None);
                return Err(err);
            }
            let modifiers_elapsed = modifiers_started.elapsed();
            let total_elapsed = total_started.elapsed();
            info!(
                target: "softkvm::latency",
                reason,
                capture_ms = latency::ms(capture_elapsed),
                send_state_ms = latency::ms(send_state_elapsed),
                seed_modifiers_ms = latency::ms(modifiers_elapsed),
                total_ms = latency::ms(total_elapsed),
                "windows remote activation latency"
            );
            info!(reason, "remote macOS control enabled");
        } else {
            self.remote_active = false;
            set_remote_active_fast(false);
            LOCAL_MOUSE_CAPTURE_FAST.store(true, Ordering::Relaxed);
            self.left_edge_armed = false;
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

    fn send_direct_motion(&self, dx: i32, dy: i32) -> Result<bool> {
        let Some(writer) = &self.direct_motion else {
            return Ok(false);
        };
        writer.send_motion_if_confirmed(dx, dy, "direct")
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

    fn send_current_modifier_state(&mut self) -> Result<()> {
        for event in self.seed_current_modifier_state() {
            self.send(HostCommand::InputImmediate(event))?;
        }
        Ok(())
    }

    fn seed_current_modifier_state(&mut self) -> Vec<InputEvent> {
        let mut events = Vec::new();

        for (modifier, vkeys) in [
            (Modifier::Control, [VK_LCONTROL.0, VK_RCONTROL.0]),
            (Modifier::Alt, [VK_LMENU.0, VK_RMENU.0]),
            (Modifier::Super, [VK_LWIN.0, VK_RWIN.0]),
            (Modifier::Shift, [VK_LSHIFT.0, VK_RSHIFT.0]),
        ] {
            let mut modifier_down = false;
            for vkey in vkeys {
                if key_down(vkey) {
                    self.pressed_modifier_keys.insert(vkey);
                    modifier_down = true;
                }
            }
            if modifier_down && self.active_modifiers.insert(modifier) {
                events.push(InputEvent::Modifier {
                    modifier,
                    state: KeyState::Down,
                });
            }
        }

        events
    }

    fn capture_local_controls(&mut self) -> Result<()> {
        let started = Instant::now();
        let cursor = current_cursor_position().unwrap_or_else(|_| POINT {
            x: unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) },
            y: unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) },
        });
        self.saved_cursor_pos = Some(cursor);
        self.saved_monitor_rect = monitor_rect_for_point(cursor);
        if !self.local_mouse_capture {
            if latency::enabled() {
                info!(
                    target: "softkvm::latency",
                    x = cursor.x,
                    y = cursor.y,
                    "windows local mouse capture disabled for VM test"
                );
            }
            return Ok(());
        }
        let clip_started = Instant::now();
        unsafe {
            if let Err(err) = clip_cursor_to_parking_box(cursor) {
                release_remote_controls();
                return Err(err);
            }
        }
        let clip_elapsed = clip_started.elapsed();
        let elapsed = started.elapsed();
        if latency::report(elapsed) {
            info!(
                target: "softkvm::latency",
                x = cursor.x,
                y = cursor.y,
                clip_ms = latency::ms(clip_elapsed),
                total_ms = latency::ms(elapsed),
                "windows capture local controls latency"
            );
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

        if restore_cursor {
            let target = match (self.saved_monitor_rect, entry_x_ratio.zip(entry_y_ratio)) {
                (Some(rect), Some((x_ratio, y_ratio))) => {
                    Some(clamp_restore_point_inside_left_edge(
                        rect,
                        point_from_rect_ratios(rect, x_ratio, y_ratio),
                    ))
                }
                (Some(rect), None) => self
                    .saved_cursor_pos
                    .map(|point| clamp_restore_point_inside_left_edge(rect, point)),
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

    fn update_left_edge_arm(&mut self) {
        if self.left_edge_armed {
            return;
        }
        if let Ok(point) = current_cursor_position() {
            let virtual_left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
            if point.x > virtual_left + EDGE_TRIGGER_PX + EDGE_REARM_PX {
                self.left_edge_armed = true;
            }
        }
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

unsafe fn clip_cursor_to_parking_box(point: POINT) -> Result<()> {
    let rect = RECT {
        left: point.x,
        top: point.y - EDGE_TRIGGER_PX,
        right: point.x + EDGE_TRIGGER_PX + 1,
        bottom: point.y + EDGE_TRIGGER_PX + 1,
    };
    unsafe { ClipCursor(Some(&rect)).context("ClipCursor parking box") }
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

/// Built-in triage for outbound WSAEADDRINUSE 10048: no PowerShell needed, so
/// execution policy and localization cannot get in the way. Everything here
/// is read-only except the throwaway bind/connect probes.
pub async fn run_port_doctor(peer: Option<String>) -> Result<()> {
    println!("== softkvm win-port-doctor ==");
    println!("Outbound connect failing with 10048/AddrInUse means the LOCAL machine could");
    println!("not allocate an ephemeral source port. This is not the Mac refusing (that");
    println!("would be 10061/10060).");
    println!();

    // 1. Ephemeral allocation self-test. Sockets are held during the loop so
    //    each bind must produce a fresh port, then all are released.
    let mut tcp_failures = 0_u32;
    let mut tcp_holds = Vec::new();
    for _ in 0..100 {
        match std::net::TcpListener::bind(("0.0.0.0", 0)) {
            Ok(listener) => tcp_holds.push(listener),
            Err(_) => tcp_failures += 1,
        }
    }
    drop(tcp_holds);
    let mut udp_failures = 0_u32;
    let mut udp_holds = Vec::new();
    for _ in 0..100 {
        match StdUdpSocket::bind("0.0.0.0:0") {
            Ok(socket) => udp_holds.push(socket),
            Err(_) => udp_failures += 1,
        }
    }
    drop(udp_holds);
    println!(
        "[1] ephemeral self-test: tcp_failures={tcp_failures}/100 udp_failures={udp_failures}/100"
    );

    // 2. Explicit fallback range availability (what the host now uses on 10048).
    let mut scan_ok = 0_u32;
    for index in 0..crate::transport::EXPLICIT_BIND_PORT_ATTEMPTS {
        let port = crate::transport::explicit_bind_candidate_port(index);
        if let Ok(listener) = std::net::TcpListener::bind(("0.0.0.0", port)) {
            drop(listener);
            scan_ok += 1;
        }
    }
    println!(
        "[2] explicit fallback range (21309-27772): {scan_ok}/{} candidates bindable",
        crate::transport::EXPLICIT_BIND_PORT_ATTEMPTS
    );
    println!();

    // 3. OS configuration as netsh reports it (raw, survives localization).
    // dynamicport prints one number per line (start, then count); excluded
    // ranges print two numbers per line (start, end).
    let (dynamic_numbers, _) = print_netsh(&["int", "ipv4", "show", "dynamicport", "tcp"]);
    let (_, excluded) = print_netsh(&["int", "ipv4", "show", "excludedportrange", "protocol=tcp"]);
    if let (Some(&start), Some(&count)) = (dynamic_numbers.first(), dynamic_numbers.get(1)) {
        let dyn_lo = start;
        let dyn_hi = start.saturating_add(count.saturating_sub(1));
        let covered: u32 = excluded
            .iter()
            .map(|&(range_start, range_end_or_count)| {
                // excludedportrange rows are (start, end); dynamicport gave (start, count).
                let range_end = range_end_or_count.max(range_start);
                let lo = range_start.max(dyn_lo);
                let hi = range_end.min(dyn_hi);
                hi.saturating_sub(lo).saturating_add(u32::from(hi >= lo))
            })
            .sum();
        let total = count.max(1);
        println!(
            "[3] dynamic range {dyn_lo}-{dyn_hi} ({total} ports); excluded ranges cover ~{covered} of them ({}%)",
            covered.saturating_mul(100) / total
        );
        if covered.saturating_mul(100) / total >= 60 {
            println!(">>> exclusion ranges (Hyper-V/WSL/WinNAT) are eating the dynamic range.");
        }
    }
    println!();

    // 4. Dynamic-range occupancy from netstat (state names are localized, so
    //    count local ports and owning PIDs instead of parsing state words).
    match std::process::Command::new("netstat")
        .args(["-ano", "-p", "tcp"])
        .output()
    {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stdout);
            let mut in_dynamic = 0_u32;
            let mut total_rows = 0_u32;
            let mut per_pid: HashMap<String, u32> = HashMap::new();
            let dyn_lo = dynamic_numbers.first().copied().unwrap_or(49152);
            for line in text.lines() {
                let tokens: Vec<&str> = line.split_whitespace().collect();
                if tokens.len() < 4 || !tokens[0].eq_ignore_ascii_case("tcp") {
                    continue;
                }
                total_rows += 1;
                if let Some(port) = tokens[1]
                    .rsplit(':')
                    .next()
                    .and_then(|p| p.parse::<u32>().ok())
                    && port >= dyn_lo
                {
                    in_dynamic += 1;
                    let pid = tokens.last().copied().unwrap_or("?").to_owned();
                    *per_pid.entry(pid).or_default() += 1;
                }
            }
            let range_size = dynamic_numbers.get(1).copied().unwrap_or(16384).max(1);
            println!(
                "[4] tcp sockets: total={total_rows}, local port in dynamic range={in_dynamic} \
                 (range holds {range_size} ports; TIME_WAIT rows can exceed that)"
            );
            let names = tasklist_names_by_pid();
            let mut top: Vec<(String, u32)> = per_pid.into_iter().collect();
            top.sort_by(|a, b| b.1.cmp(&a.1));
            for (pid, count) in top.into_iter().take(5) {
                let name = names
                    .get(&pid)
                    .map(|name| format!(" ({name})"))
                    .unwrap_or_default();
                println!("    pid {pid}{name}: {count} dynamic-range sockets");
            }
            if in_dynamic as u64 * 100 / range_size as u64 >= 60 {
                println!(
                    ">>> dynamic range is (near) exhaustion; the top pid above is the socket \
                     flood. Identify it, then restart or fix that process — this is what \
                     produced the 10048 storms."
                );
            }
        }
        Err(err) => println!("[4] netstat unavailable: {err}"),
    }
    println!();

    // 5. Duplicate softkvm processes.
    match std::process::Command::new("tasklist").output() {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stdout);
            let ours: Vec<&str> = text
                .lines()
                .filter(|line| line.to_ascii_lowercase().contains("softkvm"))
                .collect();
            println!("[5] softkvm processes: {}", ours.len());
            for line in &ours {
                println!("    {line}");
            }
            if ours.len() > 1 {
                println!(">>> more than one instance; stop the extras before testing.");
            }
        }
        Err(err) => println!("[5] tasklist unavailable: {err}"),
    }
    println!();

    // 6. Real outbound probes, both paths the host uses.
    if let Some(peer) = peer {
        print!("[6] direct connect {peer}: ");
        match tokio::time::timeout(
            Duration::from_secs(3),
            tokio::net::TcpStream::connect(peer.as_str()),
        )
        .await
        {
            Ok(Ok(stream)) => {
                println!(
                    "OK (local {})",
                    stream
                        .local_addr()
                        .map(|a| a.to_string())
                        .unwrap_or_default()
                );
            }
            Ok(Err(err)) => println!(
                "FAILED: {err} (kind={:?}, os={:?})",
                err.kind(),
                err.raw_os_error()
            ),
            Err(_) => println!("TIMEOUT after 3s"),
        }
        print!("    explicit-bind connect {peer}: ");
        match tokio::time::timeout(
            Duration::from_secs(3),
            crate::transport::connect_via_explicit_local_bind(&peer),
        )
        .await
        {
            Ok(Ok(stream)) => {
                println!(
                    "OK (local {}) — the host works on this machine via the built-in fallback",
                    stream
                        .local_addr()
                        .map(|a| a.to_string())
                        .unwrap_or_default()
                );
            }
            Ok(Err(err)) => println!(
                "FAILED: {err} (kind={:?}, os={:?})",
                err.kind(),
                err.raw_os_error()
            ),
            Err(_) => println!("TIMEOUT after 3s"),
        }
        println!();
    }

    println!("== fixes by finding ==");
    println!("* exclusions cover the dynamic range  -> reboot often reshuffles WinNAT; or");
    println!("  `net stop winnat && net start winnat` (admin); or delete stale ranges:");
    println!(
        "  `netsh int ipv4 delete excludedportrange protocol=tcp startport=N numberofports=M`"
    );
    println!("* dynamic range tiny/misconfigured    -> `netsh int ipv4 set dynamicport tcp");
    println!("  start=49152 num=16384` (admin), then reboot");
    println!("* one pid hoards dynamic-range ports  -> that process leaks sockets; restart it");
    println!("* self-test clean but softkvm fails   -> per-app WFP/AV rule: add a Defender");
    println!("  exclusion for the kit folder (tradeoff: that folder stops being scanned)");
    Ok(())
}

/// Map pid -> image name via `tasklist /fo csv /nh` (fields stay parseable on
/// localized Windows; only the labels we skip are translated).
fn tasklist_names_by_pid() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(output) = std::process::Command::new("tasklist")
        .args(["/fo", "csv", "/nh"])
        .output()
    else {
        return map;
    };
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let fields: Vec<&str> = line.trim_matches('"').split("\",\"").collect();
        if let [name, pid, ..] = fields.as_slice() {
            map.insert((*pid).to_owned(), (*name).to_owned());
        }
    }
    map
}

/// Run netsh, print raw output, and best-effort extract numbers per line
/// (localization-proof: labels change, numbers do not). Returns the flat
/// sequence of standalone numbers (for dynamicport: start, then count) and
/// the (first, second) pairs from two-number lines (excludedportrange rows).
fn print_netsh(args: &[&str]) -> (Vec<u32>, Vec<(u32, u32)>) {
    println!("--- netsh {} ---", args.join(" "));
    let output = match std::process::Command::new("netsh").args(args).output() {
        Ok(output) => output,
        Err(err) => {
            println!("netsh unavailable: {err}");
            return (Vec::new(), Vec::new());
        }
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut singles = Vec::new();
    let mut pairs = Vec::new();
    for line in text.lines() {
        println!("{line}");
        let numbers: Vec<u32> = line
            .split_whitespace()
            .filter_map(|token| token.parse::<u32>().ok())
            .collect();
        match numbers.as_slice() {
            [single] => singles.push(*single),
            [first, second] if *first >= 1024 => pairs.push((*first, *second)),
            _ => {}
        }
    }
    (singles, pairs)
}
