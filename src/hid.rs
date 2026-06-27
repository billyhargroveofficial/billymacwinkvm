#[cfg(unix)]
use anyhow::Context;
use anyhow::Result;
use serde::Serialize;
use tracing::info;

use crate::protocol::InputEvent;
#[cfg(unix)]
use crate::protocol::{KeyState, MacModifier, MacModifierPolicy, MouseButton};

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
pub async fn karabiner_sink_async() -> Result<Box<dyn HidSink>> {
    Ok(Box::new(KarabinerSink::connect().await?))
}

#[cfg(not(unix))]
pub async fn karabiner_sink_async() -> Result<Box<dyn HidSink>> {
    anyhow::bail!("Karabiner VirtualHID sink is macOS-only")
}

#[cfg(unix)]
struct KarabinerSink {
    client: KarabinerClient,
    buttons: u32,
    modifiers: u8,
    modifier_policy: MacModifierPolicy,
}

#[cfg(unix)]
impl KarabinerSink {
    async fn connect() -> Result<Self> {
        let mut client = KarabinerClient::connect(KARABINER_SOCKET).await?;
        client.initialize_keyboard().await?;
        client.initialize_pointing().await?;
        Ok(Self {
            client,
            buttons: 0,
            modifiers: 0,
            modifier_policy: MacModifierPolicy::SwapAltSuper,
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

        while dx != 0 || dy != 0 || vertical_wheel != 0 || horizontal_wheel != 0 {
            let x = take_i8_chunk(&mut dx);
            let y = take_i8_chunk(&mut dy);
            let v = take_i8_chunk(&mut vertical_wheel);
            let h = take_i8_chunk(&mut horizontal_wheel);
            self.client.post_pointing(self.buttons, x, y, v, h).await?;
        }

        Ok(())
    }

    async fn post_keyboard_state(&mut self) -> Result<()> {
        self.client
            .post_keyboard(self.modifiers, &[0_u16; 32])
            .await
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
            InputEvent::Key { .. } => {
                // Ordinary key mapping is wired after Windows scan-code capture. Modifiers are
                // enough for the first HID proof and prevent accidentally typing into apps.
                Ok(())
            }
        }
    }

    async fn reset(&mut self) -> Result<()> {
        self.buttons = 0;
        self.modifiers = 0;
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
fn take_i8_chunk(value: &mut i32) -> i8 {
    let chunk = (*value).clamp(i8::MIN as i32, i8::MAX as i32);
    *value -= chunk;
    chunk as i8
}

#[cfg(unix)]
struct KarabinerClient {
    stream: tokio::net::UnixStream,
    next_request_id: u64,
}

#[cfg(unix)]
impl KarabinerClient {
    async fn connect(path: &str) -> Result<Self> {
        let stream = tokio::net::UnixStream::connect(path)
            .await
            .with_context(|| format!("connect Karabiner socket {path}"))?;
        Ok(Self {
            stream,
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

    async fn request(&mut self, request: KarabinerRequest, payload: &[u8]) -> Result<()> {
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

        let len = u32::try_from(body.len()).context("Karabiner frame too large")?;
        tokio::io::AsyncWriteExt::write_all(&mut self.stream, &len.to_be_bytes()).await?;
        tokio::io::AsyncWriteExt::write_all(&mut self.stream, &body).await?;
        tokio::io::AsyncWriteExt::flush(&mut self.stream).await?;

        Ok(())
    }
}

#[cfg(unix)]
#[derive(Clone, Copy)]
enum KarabinerRequest {
    VirtualHidKeyboardInitialize = 1,
    VirtualHidPointingInitialize = 4,
    PostKeyboardInputReport = 7,
    PostPointingInputReport = 12,
}
