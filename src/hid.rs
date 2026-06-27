use anyhow::Result;
#[cfg(unix)]
use anyhow::{Context, bail};
use serde::Serialize;
#[cfg(unix)]
use std::collections::HashSet;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
#[cfg(unix)]
use std::time::Instant;
#[cfg(unix)]
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
#[cfg(unix)]
use tokio::sync::Mutex as AsyncMutex;
#[cfg(unix)]
use tokio::time::{Duration, sleep};
use tracing::info;
#[cfg(unix)]
use tracing::trace;
#[cfg(unix)]
use tracing::warn;

#[cfg(unix)]
use crate::latency;
use crate::protocol::InputEvent;
#[cfg(unix)]
use crate::protocol::{KeyCode, KeyState, MacModifier, MacModifierPolicy, MouseButton};

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

#[derive(Debug, Serialize)]
pub struct KarabinerProbe {
    pub ready: bool,
    pub socket_path: String,
    pub socket_exists: bool,
    pub candidates: Vec<String>,
    pub notes: Vec<String>,
}

impl KarabinerProbe {
    pub fn detect() -> Self {
        let socket_path = KARABINER_SOCKET.to_owned();
        let socket_exists = std::path::Path::new(KARABINER_SOCKET).exists();
        let candidates = candidate_paths()
            .into_iter()
            .filter(|p| std::path::Path::new(p).exists())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();

        let mut notes = Vec::new();
        if !socket_exists {
            notes.push(format!("Socket is missing: {KARABINER_SOCKET}"));
        }
        if candidates.is_empty() {
            notes.push("No Karabiner VirtualHID files found in common install paths.".to_owned());
            notes.push("Install Karabiner-DriverKit-VirtualHIDDevice or Karabiner-Elements, approve the system extension, then rerun.".to_owned());
        } else {
            notes.push("Found Karabiner-related files.".to_owned());
        }

        Self {
            ready: socket_exists && !candidates.is_empty(),
            socket_path,
            socket_exists,
            candidates,
            notes,
        }
    }
}

fn candidate_paths() -> Vec<&'static str> {
    vec![
        "/Applications/Karabiner-Elements.app",
        "/Applications/.Karabiner-VirtualHIDDevice-Manager.app",
        "/Library/Application Support/org.pqrs/Karabiner-Elements",
        "/Library/Application Support/org.pqrs/Karabiner-DriverKit-VirtualHIDDevice",
    ]
}

pub const KARABINER_SOCKET: &str =
    "/Library/Application Support/org.pqrs/tmp/rootonly/karabiner_virtual_hid_device_service.sock";

#[cfg(unix)]
static KARABINER_POINTING_BUTTONS: AtomicU32 = AtomicU32::new(0);

#[cfg(unix)]
pub async fn karabiner_sink_async() -> Result<Box<dyn HidSink>> {
    Ok(Box::new(KarabinerSink::connect().await?))
}

#[cfg(not(unix))]
pub async fn karabiner_sink_async() -> Result<Box<dyn HidSink>> {
    anyhow::bail!("Karabiner VirtualHID sink is macOS-only")
}

#[cfg(target_os = "macos")]
pub async fn native_hid_sink_async() -> Result<Box<dyn HidSink>> {
    Ok(Box::new(NativeHidSink::open()?))
}

#[cfg(not(target_os = "macos"))]
pub async fn native_hid_sink_async() -> Result<Box<dyn HidSink>> {
    anyhow::bail!("native macOS HID sink is macOS-only")
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

#[cfg(unix)]
struct KarabinerSink {
    client: KarabinerClient,
    buttons: u32,
    modifiers: u8,
    keys: HashSet<u16>,
    modifier_policy: MacModifierPolicy,
}

#[cfg(unix)]
impl KarabinerSink {
    async fn connect() -> Result<Self> {
        let mut client = KarabinerClient::connect(KARABINER_SOCKET).await?;
        client.initialize_keyboard().await?;
        client.initialize_pointing().await?;
        client.wait_ready().await?;
        let modifier_policy = MacModifierPolicy::from_env();
        info!(?modifier_policy, "using macOS modifier policy");
        Ok(Self {
            client,
            buttons: 0,
            modifiers: 0,
            keys: HashSet::new(),
            modifier_policy,
        })
    }

    async fn post_pointing_chunks(
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

        let mut reports = Vec::new();
        while dx != 0 || dy != 0 || vertical_wheel != 0 || horizontal_wheel != 0 {
            let x = take_i8_chunk(&mut dx);
            let y = take_i8_chunk(&mut dy);
            let v = take_i8_chunk(&mut vertical_wheel);
            let h = take_i8_chunk(&mut horizontal_wheel);
            reports.push((
                KARABINER_POINTING_BUTTONS.load(Ordering::Relaxed),
                x,
                y,
                v,
                h,
            ));
        }

        self.client.post_pointing_reports(&reports).await
    }

    async fn post_keyboard_state(&mut self) -> Result<()> {
        let mut keys = [0_u16; 32];
        let mut pressed = self.keys.iter().copied().collect::<Vec<_>>();
        pressed.sort_unstable();
        for (slot, key) in keys.iter_mut().zip(pressed.iter().copied()) {
            *slot = key;
        }
        if self.modifiers != 0 || !pressed.is_empty() {
            trace!(
                modifiers = self.modifiers,
                keys = ?pressed,
                "posting Karabiner keyboard state"
            );
        }
        self.client.post_keyboard(self.modifiers, &keys).await
    }
}

#[cfg(unix)]
#[async_trait::async_trait]
impl HidSink for KarabinerSink {
    async fn apply(&mut self, event: InputEvent) -> Result<()> {
        match event {
            InputEvent::MouseMotion { dx, dy } => self.post_pointing_chunks(dx, dy, 0, 0).await,
            InputEvent::MouseWheel { dx, dy } => self.post_pointing_chunks(0, 0, dy, dx).await,
            InputEvent::MouseButton { button, state } => {
                let bit = mouse_button_bit(button);
                match state {
                    KeyState::Down => self.buttons |= bit,
                    KeyState::Up => self.buttons &= !bit,
                }
                KARABINER_POINTING_BUTTONS.store(self.buttons, Ordering::Relaxed);
                self.client.post_pointing(self.buttons, 0, 0, 0, 0).await
            }
            InputEvent::Modifier { modifier, state } => {
                let bit = mac_modifier_bit(self.modifier_policy.map(modifier));
                match state {
                    KeyState::Down => self.modifiers |= bit,
                    KeyState::Up => self.modifiers &= !bit,
                }
                self.post_keyboard_state().await
            }
            InputEvent::Key { key, state } => {
                let Some(usage) = mac_key_usage(key) else {
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
                self.post_keyboard_state().await
            }
        }
    }

    async fn reset(&mut self) -> Result<()> {
        self.buttons = 0;
        KARABINER_POINTING_BUTTONS.store(0, Ordering::Relaxed);
        self.modifiers = 0;
        self.keys.clear();
        self.client.post_pointing(0, 0, 0, 0, 0).await?;
        self.post_keyboard_state().await
    }
}

#[cfg(unix)]
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

#[cfg(unix)]
fn mac_modifier_bit(modifier: MacModifier) -> u8 {
    match modifier {
        MacModifier::Control => 0x01,
        MacModifier::Shift => 0x02,
        MacModifier::Option => 0x04,
        MacModifier::Command => 0x08,
    }
}

#[cfg(unix)]
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

#[cfg(unix)]
fn take_i8_chunk(value: &mut i32) -> i8 {
    let chunk = (*value).clamp(i8::MIN as i32, i8::MAX as i32);
    *value -= chunk;
    chunk as i8
}

#[cfg(unix)]
struct KarabinerClient {
    writer: Arc<AsyncMutex<OwnedWriteHalf>>,
    status: Arc<KarabinerStatus>,
    next_request_id: u64,
}

#[cfg(unix)]
#[derive(Default)]
struct KarabinerStatus {
    keyboard_ready: AtomicBool,
    pointing_ready: AtomicBool,
}

#[cfg(unix)]
impl KarabinerClient {
    async fn connect(path: &str) -> Result<Self> {
        let stream = tokio::net::UnixStream::connect(path)
            .await
            .with_context(|| format!("connect Karabiner socket {path}"))?;
        let (reader, writer) = stream.into_split();
        let writer = Arc::new(AsyncMutex::new(writer));
        let status = Arc::new(KarabinerStatus::default());
        tokio::spawn(karabiner_reader_task(
            reader,
            writer.clone(),
            status.clone(),
        ));
        tokio::spawn(karabiner_heartbeat_task(writer.clone()));
        Ok(Self {
            writer,
            status,
            next_request_id: 1,
        })
    }

    async fn initialize_keyboard(&mut self) -> Result<()> {
        let mut payload = Vec::with_capacity(24);
        payload.extend_from_slice(&0x16c0_u64.to_le_bytes());
        payload.extend_from_slice(&0x27db_u64.to_le_bytes());
        payload.extend_from_slice(&33_u64.to_le_bytes());
        self.request(KarabinerRequest::VirtualHidKeyboardInitialize, &payload)
            .await
    }

    async fn initialize_pointing(&mut self) -> Result<()> {
        self.request(KarabinerRequest::VirtualHidPointingInitialize, &[])
            .await
    }

    async fn wait_ready(&mut self) -> Result<()> {
        for _ in 0..40 {
            self.get_status().await?;
            sleep(Duration::from_millis(50)).await;

            if self.status.keyboard_ready.load(Ordering::Relaxed)
                && self.status.pointing_ready.load(Ordering::Relaxed)
            {
                info!("Karabiner VirtualHID reports keyboard and pointing ready");
                return Ok(());
            }
        }

        warn!(
            keyboard_ready = self.status.keyboard_ready.load(Ordering::Relaxed),
            pointing_ready = self.status.pointing_ready.load(Ordering::Relaxed),
            "Karabiner VirtualHID did not report full readiness before timeout"
        );
        Ok(())
    }

    async fn get_status(&mut self) -> Result<()> {
        self.request(KarabinerRequest::GetStatus, &[]).await
    }

    async fn post_keyboard(&mut self, modifiers: u8, keys: &[u16; 32]) -> Result<()> {
        let mut payload = Vec::with_capacity(67);
        payload.push(1); // report_id
        payload.push(modifiers);
        payload.push(0); // reserved
        for key in keys {
            payload.extend_from_slice(&key.to_le_bytes());
        }
        debug_assert_eq!(payload.len(), 67);
        self.request(KarabinerRequest::PostKeyboardInputReport, &payload)
            .await
    }

    async fn post_pointing(
        &mut self,
        buttons: u32,
        x: i8,
        y: i8,
        vertical_wheel: i8,
        horizontal_wheel: i8,
    ) -> Result<()> {
        let mut payload = Vec::with_capacity(8);
        payload.extend_from_slice(&buttons.to_le_bytes());
        payload.push(x as u8);
        payload.push(y as u8);
        payload.push(vertical_wheel as u8);
        payload.push(horizontal_wheel as u8);
        debug_assert_eq!(payload.len(), 8);
        self.request(KarabinerRequest::PostPointingInputReport, &payload)
            .await
    }

    async fn post_pointing_reports(&mut self, reports: &[(u32, i8, i8, i8, i8)]) -> Result<()> {
        if reports.is_empty() {
            return Ok(());
        }

        let mut bodies = Vec::with_capacity(reports.len());
        for &(buttons, x, y, vertical_wheel, horizontal_wheel) in reports {
            let mut payload = Vec::with_capacity(8);
            payload.extend_from_slice(&buttons.to_le_bytes());
            payload.push(x as u8);
            payload.push(y as u8);
            payload.push(vertical_wheel as u8);
            payload.push(horizontal_wheel as u8);
            debug_assert_eq!(payload.len(), 8);
            bodies.push(self.request_body(KarabinerRequest::PostPointingInputReport, &payload));
        }

        self.write_request_bodies(bodies).await
    }

    async fn request(&mut self, request: KarabinerRequest, payload: &[u8]) -> Result<()> {
        let body = self.request_body(request, payload);
        self.write_request_bodies(vec![body]).await
    }

    fn request_body(&mut self, request: KarabinerRequest, payload: &[u8]) -> Vec<u8> {
        let request_id = self.next_request_id;
        self.next_request_id += 1;

        let mut karabiner_payload = Vec::with_capacity(3 + payload.len());
        karabiner_payload.extend_from_slice(&6_u16.to_le_bytes());
        karabiner_payload.push(request as u8);
        karabiner_payload.extend_from_slice(payload);

        let mut body = Vec::with_capacity(1 + 8 + karabiner_payload.len());
        body.push(4); // pqrs::unix_domain_stream::message_type::request
        body.extend_from_slice(&request_id.to_be_bytes());
        body.extend_from_slice(&karabiner_payload);
        body
    }

    async fn write_request_bodies(&self, bodies: Vec<Vec<u8>>) -> Result<()> {
        let body_count = bodies.len();
        let mut wire = Vec::new();
        for body in bodies {
            let len = u32::try_from(body.len()).context("Karabiner frame too large")?;
            wire.extend_from_slice(&len.to_be_bytes());
            wire.extend_from_slice(&body);
        }
        let bytes = wire.len();
        let lock_started = Instant::now();
        let mut writer = self.writer.lock().await;
        let lock_elapsed = lock_started.elapsed();
        let write_started = Instant::now();
        writer.write_all(&wire).await?;
        let write_elapsed = write_started.elapsed();
        if latency::report(lock_elapsed) || latency::report(write_elapsed) {
            warn!(
                target: "softkvm::latency",
                bodies = body_count,
                bytes,
                lock_ms = latency::ms(lock_elapsed),
                write_ms = latency::ms(write_elapsed),
                "Karabiner write latency"
            );
        }
        Ok(())
    }
}

#[cfg(unix)]
async fn karabiner_heartbeat_task(writer: Arc<AsyncMutex<OwnedWriteHalf>>) {
    loop {
        sleep(Duration::from_millis(2500)).await;
        let mut writer = writer.lock().await;
        if let Err(err) = write_control_frame(&mut *writer, 0, &[]).await {
            warn!(?err, "Karabiner heartbeat stopped");
            break;
        }
    }
}

#[cfg(unix)]
async fn karabiner_reader_task(
    mut reader: OwnedReadHalf,
    writer: Arc<AsyncMutex<OwnedWriteHalf>>,
    status: Arc<KarabinerStatus>,
) {
    loop {
        let body = match read_karabiner_body(&mut reader).await {
            Ok(body) => body,
            Err(err) => {
                warn!(?err, "Karabiner reader stopped");
                break;
            }
        };

        match body[0] {
            0 => {
                // heartbeat
            }
            2 => {
                let mut writer = writer.lock().await;
                if let Err(err) = write_control_frame(&mut *writer, 3, &[]).await {
                    warn!(?err, "failed to answer Karabiner health_check");
                    break;
                }
            }
            5 => {
                handle_karabiner_response(&body, &status);
            }
            _ => {
                // Ignore unrelated protocol frames.
            }
        }
    }
}

#[cfg(unix)]
fn handle_karabiner_response(body: &[u8], status: &KarabinerStatus) {
    const RESPONSE_HEADER_LEN: usize = 1 + 8;

    if body.len() < RESPONSE_HEADER_LEN {
        warn!(len = body.len(), "short Karabiner response");
        return;
    }

    let payload = &body[RESPONSE_HEADER_LEN..];
    if payload.len() % 2 != 0 {
        warn!(
            len = payload.len(),
            "invalid Karabiner status response payload"
        );
        return;
    }

    for pair in payload.chunks_exact(2) {
        let response_type = pair[0];
        let ready = pair[1] != 0;
        match response_type {
            4 => {
                if status.keyboard_ready.swap(ready, Ordering::Relaxed) != ready {
                    info!(ready, "Karabiner virtual keyboard readiness changed");
                }
            }
            5 => {
                if status.pointing_ready.swap(ready, Ordering::Relaxed) != ready {
                    info!(ready, "Karabiner virtual pointing readiness changed");
                }
            }
            _ => {}
        }
    }
}

#[cfg(unix)]
async fn read_karabiner_body<R>(reader: &mut R) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut len = [0_u8; 4];
    reader.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 {
        bail!("Karabiner frame is empty");
    }

    let mut body = vec![0_u8; len];
    reader.read_exact(&mut body).await?;
    Ok(body)
}

#[cfg(unix)]
async fn write_control_frame<W>(writer: &mut W, message_type: u8, payload: &[u8]) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut body = Vec::with_capacity(1 + payload.len());
    body.push(message_type);
    body.extend_from_slice(payload);
    write_karabiner_body(writer, &body).await
}

#[cfg(unix)]
async fn write_karabiner_body<W>(writer: &mut W, body: &[u8]) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let len = u32::try_from(body.len()).context("Karabiner frame too large")?;
    let mut wire = Vec::with_capacity(4 + body.len());
    wire.extend_from_slice(&len.to_be_bytes());
    wire.extend_from_slice(body);
    writer.write_all(&wire).await?;
    Ok(())
}

#[cfg(unix)]
#[derive(Clone, Copy)]
enum KarabinerRequest {
    GetStatus = 0,
    VirtualHidKeyboardInitialize = 1,
    VirtualHidPointingInitialize = 4,
    PostKeyboardInputReport = 7,
    PostPointingInputReport = 12,
}
