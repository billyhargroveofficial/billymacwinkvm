use anyhow::Result;
#[cfg(target_os = "macos")]
use anyhow::bail;
#[cfg(target_os = "macos")]
use std::collections::HashSet;
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicU32, Ordering};
#[cfg(target_os = "macos")]
use std::time::Instant;
use tracing::info;
#[cfg(target_os = "macos")]
use tracing::warn;

#[cfg(target_os = "macos")]
use crate::latency;
use crate::protocol::InputEvent;
#[cfg(target_os = "macos")]
use crate::protocol::{KeyCode, KeyState, MacModifier, MacModifierPolicy, Modifier, MouseButton};

#[async_trait::async_trait]
pub trait HidSink: Send {
    async fn apply(&mut self, event: InputEvent) -> Result<()>;
    async fn reset(&mut self) -> Result<()>;
}

#[derive(Default)]
pub struct LogSink {
    events_seen: u64,
}

#[async_trait::async_trait]
impl HidSink for LogSink {
    async fn apply(&mut self, event: InputEvent) -> Result<()> {
        self.events_seen += 1;
        info!(self.events_seen, ?event, "input event");
        Ok(())
    }

    async fn reset(&mut self) -> Result<()> {
        info!("input reset");
        Ok(())
    }
}

#[cfg(target_os = "macos")]
static CGEVENT_POINTING_BUTTONS: AtomicU32 = AtomicU32::new(0);
#[cfg(target_os = "macos")]
static CGEVENT_CURSOR_POSITION: std::sync::Mutex<Option<CGPoint>> = std::sync::Mutex::new(None);
#[cfg(target_os = "macos")]
static CGEVENT_POINTER_SPEED: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
#[cfg(target_os = "macos")]
static CGEVENT_MOTION_METHOD: std::sync::OnceLock<CgEventMotionMethod> = std::sync::OnceLock::new();
#[cfg(target_os = "macos")]
static CGEVENT_TAP: std::sync::OnceLock<CgEventTap> = std::sync::OnceLock::new();

#[cfg(target_os = "macos")]
pub async fn native_hid_sink_async() -> Result<Box<dyn HidSink>> {
    Ok(Box::new(NativeHidSink::open()?))
}

#[cfg(not(target_os = "macos"))]
pub async fn native_hid_sink_async() -> Result<Box<dyn HidSink>> {
    anyhow::bail!("native macOS HID sink is macOS-only")
}

#[cfg(target_os = "macos")]
pub async fn cgevent_sink_async() -> Result<Box<dyn HidSink>> {
    Ok(Box::new(CgEventSink::connect().await?))
}

#[cfg(not(target_os = "macos"))]
pub async fn cgevent_sink_async() -> Result<Box<dyn HidSink>> {
    anyhow::bail!("CGEvent sink is macOS-only")
}

#[cfg(target_os = "macos")]
pub fn cgevent_note_cursor_position(x: f64, y: f64) {
    let point = CgEventSink::clamp_to_main_display(CGPoint { x, y });
    if let Ok(mut position) = CGEVENT_CURSOR_POSITION.lock() {
        *position = Some(point);
    }
}

#[cfg(target_os = "macos")]
pub fn cgevent_cached_cursor_position() -> Option<(f64, f64)> {
    let mut position = CGEVENT_CURSOR_POSITION.lock().ok()?;
    let point = CgEventSink::clamp_to_main_display((*position)?);
    *position = Some(point);
    Some((point.x, point.y))
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CgEventMotionMethod {
    Warp,
    Event,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CgEventTap {
    Hid,
    Session,
    AnnotatedSession,
}

#[cfg(target_os = "macos")]
impl CgEventTap {
    fn value(self) -> u32 {
        match self {
            Self::Hid => CG_HID_EVENT_TAP,
            Self::Session => CG_SESSION_EVENT_TAP,
            Self::AnnotatedSession => CG_ANNOTATED_SESSION_EVENT_TAP,
        }
    }
}

#[cfg(target_os = "macos")]
struct CgEventSink {
    buttons: u32,
    modifiers: u64,
    modifier_policy: MacModifierPolicy,
}

#[cfg(target_os = "macos")]
impl CgEventSink {
    async fn connect() -> Result<Self> {
        let modifier_policy = MacModifierPolicy::from_env();
        let pointer_speed = cgevent_pointer_speed();
        let motion_method = cgevent_motion_method();
        let event_tap = cgevent_tap();
        unsafe {
            CGSetLocalEventsSuppressionInterval(0.0);
        }
        info!(
            ?modifier_policy,
            pointer_speed,
            ?motion_method,
            ?event_tap,
            scroll_inverted = true,
            "using CGEvent mouse and keyboard sink"
        );
        Ok(Self {
            buttons: 0,
            modifiers: 0,
            modifier_policy,
        })
    }

    fn read_current_position() -> Option<CGPoint> {
        let event = unsafe { CGEventCreate(std::ptr::null()) };
        if event.is_null() {
            return None;
        }
        let point = unsafe { CGEventGetLocation(event) };
        unsafe { CFRelease(event.cast_const()) };
        Some(point)
    }

    fn cached_position() -> Option<CGPoint> {
        let mut cached = CGEVENT_CURSOR_POSITION.lock().ok()?;
        if let Some(point) = *cached {
            let point = Self::clamp_to_main_display(point);
            *cached = Some(point);
            return Some(point);
        }
        let point = Self::clamp_to_main_display(Self::read_current_position()?);
        *cached = Some(point);
        Some(point)
    }

    fn advance_position(dx: i32, dy: i32) -> Option<CGPoint> {
        let mut cached = CGEVENT_CURSOR_POSITION.lock().ok()?;
        let current = match *cached {
            Some(point) => point,
            None => {
                let point = Self::read_current_position()?;
                *cached = Some(point);
                point
            }
        };
        let speed = cgevent_pointer_speed();
        let point = CGPoint {
            x: current.x + f64::from(dx) * speed,
            y: current.y + f64::from(dy) * speed,
        };
        let point = Self::clamp_to_main_display(point);
        *cached = Some(point);
        Some(point)
    }

    fn clamp_to_main_display(point: CGPoint) -> CGPoint {
        let bounds = unsafe { CGDisplayBounds(CGMainDisplayID()) };
        let min_x = bounds.origin.x;
        let max_x = bounds.origin.x + bounds.size.width - 1.0;
        let min_y = bounds.origin.y;
        let max_y = bounds.origin.y + bounds.size.height - 1.0;
        let clamped = CGPoint {
            x: point.x.clamp(min_x, max_x),
            y: point.y.clamp(min_y, max_y),
        };
        if (clamped.x - point.x).abs() > f64::EPSILON || (clamped.y - point.y).abs() > f64::EPSILON
        {
            warn!(
                x = point.x,
                y = point.y,
                clamped_x = clamped.x,
                clamped_y = clamped.y,
                "clamped CGEvent cursor position to main display"
            );
        }
        clamped
    }

    fn post_motion(&self, dx: i32, dy: i32) -> Result<()> {
        let Some(point) = Self::advance_position(dx, dy) else {
            bail!("CGEventCreate returned null while reading mouse position");
        };
        let buttons = CGEVENT_POINTING_BUTTONS.load(Ordering::Relaxed);
        let event_type = if buttons & mouse_button_bit(MouseButton::Left) != 0 {
            CG_EVENT_LEFT_MOUSE_DRAGGED
        } else if buttons & mouse_button_bit(MouseButton::Right) != 0 {
            CG_EVENT_RIGHT_MOUSE_DRAGGED
        } else if buttons != 0 {
            CG_EVENT_OTHER_MOUSE_DRAGGED
        } else {
            CG_EVENT_MOUSE_MOVED
        };
        if buttons == 0 && cgevent_motion_method() == CgEventMotionMethod::Warp {
            return Self::post_warp_motion(point);
        }
        self.post_mouse_event(event_type, point, CG_MOUSE_BUTTON_LEFT)
    }

    fn post_warp_motion(point: CGPoint) -> Result<()> {
        let started = Instant::now();
        let status = unsafe { CGWarpMouseCursorPosition(point) };
        let elapsed = started.elapsed();
        if status != 0 {
            bail!("CGWarpMouseCursorPosition failed: {status}");
        }
        cgevent_note_cursor_position(point.x, point.y);
        if latency::report(elapsed) {
            info!(
                target: "softkvm::latency",
                x = point.x,
                y = point.y,
                elapsed_ms = latency::ms(elapsed),
                "cgevent warp motion latency"
            );
        }
        Ok(())
    }

    fn post_button(&mut self, button: MouseButton, state: KeyState) -> Result<()> {
        let bit = mouse_button_bit(button);
        match state {
            KeyState::Down => self.buttons |= bit,
            KeyState::Up => self.buttons &= !bit,
        }
        CGEVENT_POINTING_BUTTONS.store(self.buttons, Ordering::Relaxed);
        let Some(point) = Self::cached_position() else {
            bail!("CGEventCreate returned null while reading mouse position");
        };
        let (event_type, cg_button) = match (button, state) {
            (MouseButton::Left, KeyState::Down) => (CG_EVENT_LEFT_MOUSE_DOWN, CG_MOUSE_BUTTON_LEFT),
            (MouseButton::Left, KeyState::Up) => (CG_EVENT_LEFT_MOUSE_UP, CG_MOUSE_BUTTON_LEFT),
            (MouseButton::Right, KeyState::Down) => {
                (CG_EVENT_RIGHT_MOUSE_DOWN, CG_MOUSE_BUTTON_RIGHT)
            }
            (MouseButton::Right, KeyState::Up) => (CG_EVENT_RIGHT_MOUSE_UP, CG_MOUSE_BUTTON_RIGHT),
            (MouseButton::Middle, KeyState::Down) => {
                (CG_EVENT_OTHER_MOUSE_DOWN, CG_MOUSE_BUTTON_CENTER)
            }
            (MouseButton::Middle, KeyState::Up) => {
                (CG_EVENT_OTHER_MOUSE_UP, CG_MOUSE_BUTTON_CENTER)
            }
            (MouseButton::Back | MouseButton::Forward, KeyState::Down) => {
                (CG_EVENT_OTHER_MOUSE_DOWN, CG_MOUSE_BUTTON_CENTER)
            }
            (MouseButton::Back | MouseButton::Forward, KeyState::Up) => {
                (CG_EVENT_OTHER_MOUSE_UP, CG_MOUSE_BUTTON_CENTER)
            }
        };
        self.post_mouse_event(event_type, point, cg_button)
    }

    fn post_mouse_event(&self, event_type: u32, point: CGPoint, button: u32) -> Result<()> {
        let started = Instant::now();
        let event = unsafe { CGEventCreateMouseEvent(std::ptr::null(), event_type, point, button) };
        if event.is_null() {
            bail!("CGEventCreateMouseEvent returned null");
        }
        let create_elapsed = started.elapsed();
        let post_started = Instant::now();
        unsafe {
            CGEventPost(cgevent_tap().value(), event);
            CFRelease(event.cast_const());
        }
        let post_elapsed = post_started.elapsed();
        let total_elapsed = started.elapsed();
        if latency::report(total_elapsed) {
            info!(
                target: "softkvm::latency",
                event_type,
                button,
                x = point.x,
                y = point.y,
                create_ms = latency::ms(create_elapsed),
                post_ms = latency::ms(post_elapsed),
                total_ms = latency::ms(total_elapsed),
                "cgevent mouse post latency"
            );
        }
        Ok(())
    }

    fn post_wheel(&self, dx: i32, dy: i32) -> Result<()> {
        let dx = -dx;
        let dy = -dy;
        let event = unsafe {
            CGEventCreateScrollWheelEvent(std::ptr::null(), CG_SCROLL_EVENT_UNIT_LINE, 2, dy, dx, 0)
        };
        if event.is_null() {
            bail!("CGEventCreateScrollWheelEvent returned null");
        }
        unsafe {
            CGEventPost(cgevent_tap().value(), event);
            CFRelease(event.cast_const());
        }
        Ok(())
    }

    fn post_modifier(&mut self, modifier: Modifier, state: KeyState) -> Result<()> {
        let mapped = self.modifier_policy.map(modifier);
        let flag = cg_modifier_flag(mapped);
        match state {
            KeyState::Down => self.modifiers |= flag,
            KeyState::Up => self.modifiers &= !flag,
        }
        self.post_keycode(cg_modifier_keycode(mapped), state)
    }

    fn post_key(&self, key: KeyCode, state: KeyState) -> Result<()> {
        let Some(keycode) = cg_keycode_for_key(key) else {
            return Ok(());
        };
        self.post_keycode(keycode, state)
    }

    fn post_keycode(&self, keycode: u16, state: KeyState) -> Result<()> {
        let down = matches!(state, KeyState::Down);
        let event = unsafe { CGEventCreateKeyboardEvent(std::ptr::null(), keycode, down) };
        if event.is_null() {
            bail!("CGEventCreateKeyboardEvent returned null");
        }
        unsafe {
            CGEventSetFlags(event, self.modifiers);
            CGEventPost(cgevent_tap().value(), event);
            CFRelease(event.cast_const());
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
#[async_trait::async_trait]
impl HidSink for CgEventSink {
    async fn apply(&mut self, event: InputEvent) -> Result<()> {
        match event {
            InputEvent::MouseMotion { dx, dy } => self.post_motion(dx, dy),
            InputEvent::MouseWheel { dx, dy } => self.post_wheel(dx, dy),
            InputEvent::MouseButton { button, state } => self.post_button(button, state),
            InputEvent::Modifier { modifier, state } => self.post_modifier(modifier, state),
            InputEvent::Key { key, state } => self.post_key(key, state),
        }
    }

    async fn reset(&mut self) -> Result<()> {
        self.buttons = 0;
        CGEVENT_POINTING_BUTTONS.store(0, Ordering::Relaxed);
        if let Ok(mut position) = CGEVENT_CURSOR_POSITION.lock() {
            *position = None;
        }
        self.modifiers = 0;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
struct NativeHidSink {
    mouse: NativeHidUserDevice,
    keyboard: NativeHidUserDevice,
    buttons: u8,
    modifiers: u8,
    keys: HashSet<u8>,
    modifier_policy: MacModifierPolicy,
}

#[cfg(target_os = "macos")]
impl NativeHidSink {
    fn open() -> Result<Self> {
        let mouse = NativeHidUserDevice::create(
            "softkvm Native Mouse",
            0x01,
            0x02,
            NATIVE_MOUSE_REPORT_DESCRIPTOR,
        )?;
        let keyboard = NativeHidUserDevice::create(
            "softkvm Native Keyboard",
            0x01,
            0x06,
            NATIVE_KEYBOARD_REPORT_DESCRIPTOR,
        )?;
        let modifier_policy = MacModifierPolicy::from_env();
        info!(?modifier_policy, "using native macOS virtual HID sink");
        Ok(Self {
            mouse,
            keyboard,
            buttons: 0,
            modifiers: 0,
            keys: HashSet::new(),
            modifier_policy,
        })
    }

    fn post_mouse_chunks(
        &mut self,
        dx: i32,
        dy: i32,
        vertical_wheel: i32,
        horizontal_wheel: i32,
    ) -> Result<()> {
        let mut dx = dx;
        let mut dy = dy;
        let mut vertical_wheel = vertical_wheel;
        let mut horizontal_wheel = horizontal_wheel;

        while dx != 0 || dy != 0 || vertical_wheel != 0 || horizontal_wheel != 0 {
            let report = [
                1,
                self.buttons,
                take_i8_chunk(&mut dx) as u8,
                take_i8_chunk(&mut dy) as u8,
                take_i8_chunk(&mut vertical_wheel) as u8,
                take_i8_chunk(&mut horizontal_wheel) as u8,
            ];
            self.mouse.post_report(&report)?;
        }
        Ok(())
    }

    fn post_mouse_state(&mut self) -> Result<()> {
        self.mouse.post_report(&[1, self.buttons, 0, 0, 0, 0])
    }

    fn post_keyboard_state(&mut self) -> Result<()> {
        let mut report = [0_u8; 9];
        report[0] = 1;
        report[1] = self.modifiers;
        let mut pressed = self.keys.iter().copied().collect::<Vec<_>>();
        pressed.sort_unstable();
        for (slot, key) in report[3..].iter_mut().zip(pressed.iter().copied()) {
            *slot = key;
        }
        self.keyboard.post_report(&report)
    }
}

#[cfg(target_os = "macos")]
#[async_trait::async_trait]
impl HidSink for NativeHidSink {
    async fn apply(&mut self, event: InputEvent) -> Result<()> {
        match event {
            InputEvent::MouseMotion { dx, dy } => self.post_mouse_chunks(dx, dy, 0, 0),
            InputEvent::MouseWheel { dx, dy } => self.post_mouse_chunks(0, 0, dy, dx),
            InputEvent::MouseButton { button, state } => {
                let bit = mouse_button_bit_u8(button);
                match state {
                    KeyState::Down => self.buttons |= bit,
                    KeyState::Up => self.buttons &= !bit,
                }
                self.post_mouse_state()
            }
            InputEvent::Modifier { modifier, state } => {
                let bit = mac_modifier_bit(self.modifier_policy.map(modifier));
                match state {
                    KeyState::Down => self.modifiers |= bit,
                    KeyState::Up => self.modifiers &= !bit,
                }
                self.post_keyboard_state()
            }
            InputEvent::Key { key, state } => {
                let Some(usage) = mac_key_usage_u8(key) else {
                    return Ok(());
                };
                match state {
                    KeyState::Down => {
                        self.keys.insert(usage);
                    }
                    KeyState::Up => {
                        self.keys.remove(&usage);
                    }
                }
                self.post_keyboard_state()
            }
        }
    }

    async fn reset(&mut self) -> Result<()> {
        self.buttons = 0;
        self.modifiers = 0;
        self.keys.clear();
        self.post_mouse_state()?;
        self.post_keyboard_state()
    }
}

#[cfg(target_os = "macos")]
fn mouse_button_bit_u8(button: MouseButton) -> u8 {
    mouse_button_bit(button) as u8
}

#[cfg(target_os = "macos")]
fn mac_key_usage_u8(key: KeyCode) -> Option<u8> {
    let usage = mac_key_usage(key)?;
    u8::try_from(usage).ok()
}

#[cfg(target_os = "macos")]
struct NativeHidUserDevice {
    device: IOHIDUserDeviceRef,
}

#[cfg(target_os = "macos")]
unsafe impl Send for NativeHidUserDevice {}

#[cfg(target_os = "macos")]
impl NativeHidUserDevice {
    fn create(
        product: &'static str,
        primary_usage_page: i32,
        primary_usage: i32,
        report_descriptor: &[u8],
    ) -> Result<Self> {
        use core_foundation::base::{TCFType, kCFAllocatorDefault};
        use core_foundation::data::CFData;
        use core_foundation::dictionary::CFDictionary;
        use core_foundation::number::CFNumber;
        use core_foundation::string::CFString;

        let keys = [
            CFString::from_static_string("ReportDescriptor"),
            CFString::from_static_string("VendorID"),
            CFString::from_static_string("ProductID"),
            CFString::from_static_string("Product"),
            CFString::from_static_string("Transport"),
            CFString::from_static_string("PrimaryUsagePage"),
            CFString::from_static_string("PrimaryUsage"),
        ];
        let descriptor = CFData::from_buffer(report_descriptor);
        let vendor_id = CFNumber::from(0x16c0_i32);
        let product_id = CFNumber::from(if primary_usage == 0x02 {
            0x27db_i32
        } else {
            0x27dc_i32
        });
        let product = CFString::from_static_string(product);
        let transport = CFString::from_static_string("Virtual");
        let usage_page = CFNumber::from(primary_usage_page);
        let usage = CFNumber::from(primary_usage);
        let values = [
            descriptor.as_CFType(),
            vendor_id.as_CFType(),
            product_id.as_CFType(),
            product.as_CFType(),
            transport.as_CFType(),
            usage_page.as_CFType(),
            usage.as_CFType(),
        ];
        let pairs = keys
            .into_iter()
            .zip(values)
            .collect::<Vec<(CFString, core_foundation::base::CFType)>>();
        let properties = CFDictionary::from_CFType_pairs(&pairs);
        let device = unsafe {
            IOHIDUserDeviceCreateWithProperties(
                kCFAllocatorDefault,
                properties.as_concrete_TypeRef(),
                0,
            )
        };
        if device.is_null() {
            bail!(
                "IOHIDUserDeviceCreateWithProperties returned null for {product}; likely missing com.apple.developer.hid.virtual.device entitlement"
            );
        }
        Ok(Self { device })
    }

    fn post_report(&self, report: &[u8]) -> Result<()> {
        let started = Instant::now();
        let status = unsafe {
            IOHIDUserDeviceHandleReportWithTimeStamp(
                self.device,
                mach_absolute_time(),
                report.as_ptr(),
                report.len() as _,
            )
        };
        if status != 0 {
            bail!("IOHIDUserDeviceHandleReportWithTimeStamp failed: 0x{status:08x}");
        }
        let elapsed = started.elapsed();
        if latency::report(elapsed) {
            info!(
                target: "softkvm::latency",
                bytes = report.len(),
                elapsed_ms = latency::ms(elapsed),
                "native macOS HID report latency"
            );
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl Drop for NativeHidUserDevice {
    fn drop(&mut self) {
        unsafe {
            IOHIDUserDeviceCancel(self.device);
            core_foundation::base::CFRelease(self.device.cast_const());
        }
    }
}

#[cfg(target_os = "macos")]
type IOHIDUserDeviceRef = *mut std::ffi::c_void;

#[cfg(target_os = "macos")]
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOHIDUserDeviceCreateWithProperties(
        allocator: core_foundation::base::CFAllocatorRef,
        properties: core_foundation::dictionary::CFDictionaryRef,
        options: u32,
    ) -> IOHIDUserDeviceRef;
    fn IOHIDUserDeviceHandleReportWithTimeStamp(
        device: IOHIDUserDeviceRef,
        timestamp: u64,
        report: *const u8,
        report_length: core_foundation::base::CFIndex,
    ) -> i32;
    fn IOHIDUserDeviceCancel(device: IOHIDUserDeviceRef);
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn mach_absolute_time() -> u64;
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

#[cfg(target_os = "macos")]
const CG_HID_EVENT_TAP: u32 = 0;
#[cfg(target_os = "macos")]
const CG_SESSION_EVENT_TAP: u32 = 1;
#[cfg(target_os = "macos")]
const CG_ANNOTATED_SESSION_EVENT_TAP: u32 = 2;
#[cfg(target_os = "macos")]
const CG_EVENT_LEFT_MOUSE_DOWN: u32 = 1;
#[cfg(target_os = "macos")]
const CG_EVENT_LEFT_MOUSE_UP: u32 = 2;
#[cfg(target_os = "macos")]
const CG_EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
#[cfg(target_os = "macos")]
const CG_EVENT_RIGHT_MOUSE_UP: u32 = 4;
#[cfg(target_os = "macos")]
const CG_EVENT_MOUSE_MOVED: u32 = 5;
#[cfg(target_os = "macos")]
const CG_EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
#[cfg(target_os = "macos")]
const CG_EVENT_RIGHT_MOUSE_DRAGGED: u32 = 7;
#[cfg(target_os = "macos")]
const CG_EVENT_OTHER_MOUSE_DOWN: u32 = 25;
#[cfg(target_os = "macos")]
const CG_EVENT_OTHER_MOUSE_UP: u32 = 26;
#[cfg(target_os = "macos")]
const CG_EVENT_OTHER_MOUSE_DRAGGED: u32 = 27;
#[cfg(target_os = "macos")]
const CG_MOUSE_BUTTON_LEFT: u32 = 0;
#[cfg(target_os = "macos")]
const CG_MOUSE_BUTTON_RIGHT: u32 = 1;
#[cfg(target_os = "macos")]
const CG_MOUSE_BUTTON_CENTER: u32 = 2;
#[cfg(target_os = "macos")]
const CG_SCROLL_EVENT_UNIT_LINE: u32 = 1;
#[cfg(target_os = "macos")]
const CG_EVENT_FLAG_MASK_SHIFT: u64 = 0x0002_0000;
#[cfg(target_os = "macos")]
const CG_EVENT_FLAG_MASK_CONTROL: u64 = 0x0004_0000;
#[cfg(target_os = "macos")]
const CG_EVENT_FLAG_MASK_ALTERNATE: u64 = 0x0008_0000;
#[cfg(target_os = "macos")]
const CG_EVENT_FLAG_MASK_COMMAND: u64 = 0x0010_0000;

#[cfg(target_os = "macos")]
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGEventCreate(source: *const std::ffi::c_void) -> *mut std::ffi::c_void;
    fn CGEventGetLocation(event: *mut std::ffi::c_void) -> CGPoint;
    fn CGEventCreateMouseEvent(
        source: *const std::ffi::c_void,
        mouse_type: u32,
        mouse_cursor_position: CGPoint,
        mouse_button: u32,
    ) -> *mut std::ffi::c_void;
    fn CGEventCreateScrollWheelEvent(
        source: *const std::ffi::c_void,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> *mut std::ffi::c_void;
    fn CGEventCreateKeyboardEvent(
        source: *const std::ffi::c_void,
        virtual_key: u16,
        key_down: bool,
    ) -> *mut std::ffi::c_void;
    fn CGEventSetFlags(event: *mut std::ffi::c_void, flags: u64);
    fn CGWarpMouseCursorPosition(new_cursor_position: CGPoint) -> i32;
    fn CGSetLocalEventsSuppressionInterval(seconds: f64);
    fn CGEventPost(tap: u32, event: *mut std::ffi::c_void);
    fn CFRelease(cf: *const std::ffi::c_void);
}

#[cfg(target_os = "macos")]
const NATIVE_MOUSE_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x85, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01,
    0x29, 0x05, 0x15, 0x00, 0x25, 0x01, 0x95, 0x05, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x03,
    0x81, 0x03, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08,
    0x95, 0x03, 0x81, 0x06, 0x05, 0x0c, 0x0a, 0x38, 0x02, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95,
    0x01, 0x81, 0x06, 0xc0, 0xc0,
];

#[cfg(target_os = "macos")]
const NATIVE_KEYBOARD_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x85, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00,
    0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x03, 0x95, 0x06,
    0x75, 0x08, 0x15, 0x00, 0x26, 0xff, 0x00, 0x05, 0x07, 0x19, 0x00, 0x2a, 0xff, 0x00, 0x81, 0x00,
    0xc0,
];

#[cfg(target_os = "macos")]
fn mouse_button_bit(button: MouseButton) -> u32 {
    let one_based = match button {
        MouseButton::Left => 1,
        MouseButton::Right => 2,
        MouseButton::Middle => 3,
        MouseButton::Back => 4,
        MouseButton::Forward => 5,
    };
    1 << (one_based - 1)
}

#[cfg(target_os = "macos")]
fn mac_modifier_bit(modifier: MacModifier) -> u8 {
    match modifier {
        MacModifier::Control => 0x01,
        MacModifier::Shift => 0x02,
        MacModifier::Option => 0x04,
        MacModifier::Command => 0x08,
    }
}

#[cfg(target_os = "macos")]
fn cgevent_pointer_speed() -> f64 {
    *CGEVENT_POINTER_SPEED.get_or_init(|| {
        std::env::var("SOFTKVM_CGEVENT_POINTER_SPEED")
            .ok()
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite())
            .unwrap_or(1.0)
            .clamp(0.05, 4.0)
    })
}

#[cfg(target_os = "macos")]
fn cgevent_motion_method() -> CgEventMotionMethod {
    *CGEVENT_MOTION_METHOD.get_or_init(|| {
        match std::env::var("SOFTKVM_CGEVENT_MOTION_METHOD")
            .unwrap_or_else(|_| "event".to_owned())
            .to_ascii_lowercase()
            .as_str()
        {
            "event" | "mouse-event" | "cgevent" => CgEventMotionMethod::Event,
            "warp" | "cursor-warp" => CgEventMotionMethod::Warp,
            other => {
                warn!(
                    value = other,
                    "unknown SOFTKVM_CGEVENT_MOTION_METHOD; using event"
                );
                CgEventMotionMethod::Event
            }
        }
    })
}

#[cfg(target_os = "macos")]
fn cgevent_tap() -> CgEventTap {
    *CGEVENT_TAP.get_or_init(|| {
        match std::env::var("SOFTKVM_CGEVENT_TAP")
            .unwrap_or_else(|_| "annotated-session".to_owned())
            .to_ascii_lowercase()
            .as_str()
        {
            "hid" | "hid-event-tap" => CgEventTap::Hid,
            "session" | "session-event-tap" => CgEventTap::Session,
            "annotated" | "annotated-session" | "annotated-session-event-tap" => {
                CgEventTap::AnnotatedSession
            }
            other => {
                warn!(
                    value = other,
                    "unknown SOFTKVM_CGEVENT_TAP; using annotated-session"
                );
                CgEventTap::AnnotatedSession
            }
        }
    })
}

#[cfg(target_os = "macos")]
fn cg_modifier_flag(modifier: MacModifier) -> u64 {
    match modifier {
        MacModifier::Control => CG_EVENT_FLAG_MASK_CONTROL,
        MacModifier::Shift => CG_EVENT_FLAG_MASK_SHIFT,
        MacModifier::Option => CG_EVENT_FLAG_MASK_ALTERNATE,
        MacModifier::Command => CG_EVENT_FLAG_MASK_COMMAND,
    }
}

#[cfg(target_os = "macos")]
fn cg_modifier_keycode(modifier: MacModifier) -> u16 {
    match modifier {
        MacModifier::Command => 55,
        MacModifier::Shift => 56,
        MacModifier::Option => 58,
        MacModifier::Control => 59,
    }
}

#[cfg(target_os = "macos")]
fn cg_keycode_for_key(key: KeyCode) -> Option<u16> {
    match key {
        KeyCode::Backslash => Some(42),
        KeyCode::Escape => Some(53),
        KeyCode::Space => Some(49),
        KeyCode::Enter => Some(36),
        KeyCode::Tab => Some(48),
        KeyCode::Usb(usage) => cg_keycode_for_usb_usage(usage),
        KeyCode::Other(_) => None,
    }
}

#[cfg(target_os = "macos")]
fn cg_keycode_for_usb_usage(usage: u16) -> Option<u16> {
    match usage {
        0x04 => Some(0),   // A
        0x05 => Some(11),  // B
        0x06 => Some(8),   // C
        0x07 => Some(2),   // D
        0x08 => Some(14),  // E
        0x09 => Some(3),   // F
        0x0a => Some(5),   // G
        0x0b => Some(4),   // H
        0x0c => Some(34),  // I
        0x0d => Some(38),  // J
        0x0e => Some(40),  // K
        0x0f => Some(37),  // L
        0x10 => Some(46),  // M
        0x11 => Some(45),  // N
        0x12 => Some(31),  // O
        0x13 => Some(35),  // P
        0x14 => Some(12),  // Q
        0x15 => Some(15),  // R
        0x16 => Some(1),   // S
        0x17 => Some(17),  // T
        0x18 => Some(32),  // U
        0x19 => Some(9),   // V
        0x1a => Some(13),  // W
        0x1b => Some(7),   // X
        0x1c => Some(16),  // Y
        0x1d => Some(6),   // Z
        0x1e => Some(18),  // 1
        0x1f => Some(19),  // 2
        0x20 => Some(20),  // 3
        0x21 => Some(21),  // 4
        0x22 => Some(23),  // 5
        0x23 => Some(22),  // 6
        0x24 => Some(26),  // 7
        0x25 => Some(28),  // 8
        0x26 => Some(25),  // 9
        0x27 => Some(29),  // 0
        0x28 => Some(36),  // Return
        0x29 => Some(53),  // Escape
        0x2a => Some(51),  // Delete/backspace
        0x2b => Some(48),  // Tab
        0x2c => Some(49),  // Space
        0x2d => Some(27),  // -
        0x2e => Some(24),  // =
        0x2f => Some(33),  // [
        0x30 => Some(30),  // ]
        0x31 => Some(42),  // \
        0x32 => Some(10),  // Non-US #
        0x33 => Some(41),  // ;
        0x34 => Some(39),  // '
        0x35 => Some(50),  // `
        0x36 => Some(43),  // ,
        0x37 => Some(47),  // .
        0x38 => Some(44),  // /
        0x39 => Some(57),  // Caps lock
        0x3a => Some(122), // F1
        0x3b => Some(120), // F2
        0x3c => Some(99),  // F3
        0x3d => Some(118), // F4
        0x3e => Some(96),  // F5
        0x3f => Some(97),  // F6
        0x40 => Some(98),  // F7
        0x41 => Some(100), // F8
        0x42 => Some(101), // F9
        0x43 => Some(109), // F10
        0x44 => Some(103), // F11
        0x45 => Some(111), // F12
        0x4a => Some(115), // Home
        0x4b => Some(116), // Page up
        0x4c => Some(117), // Forward delete
        0x4d => Some(119), // End
        0x4e => Some(121), // Page down
        0x4f => Some(124), // Right
        0x50 => Some(123), // Left
        0x51 => Some(125), // Down
        0x52 => Some(126), // Up
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn mac_key_usage(key: KeyCode) -> Option<u16> {
    match key {
        KeyCode::Backslash => Some(0x31),
        KeyCode::Escape => Some(0x29),
        KeyCode::Space => Some(0x2c),
        KeyCode::Enter => Some(0x28),
        KeyCode::Tab => Some(0x2b),
        KeyCode::Usb(usage) if usage != 0 => Some(usage),
        KeyCode::Usb(_) | KeyCode::Other(_) => None,
    }
}

#[cfg(target_os = "macos")]
fn take_i8_chunk(value: &mut i32) -> i8 {
    let chunk = (*value).clamp(i8::MIN as i32, i8::MAX as i32);
    *value -= chunk;
    chunk as i8
}
