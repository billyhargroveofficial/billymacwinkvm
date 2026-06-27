use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 0;
pub const MOTION_DATAGRAM_LEN: usize = 24;
const MOTION_DATAGRAM_MAGIC: &[u8; 4] = b"SKM1";
const MOTION_DATAGRAM_TYPE_MOUSE_MOTION: u8 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Frame {
    pub session_id: Uuid,
    pub seq: u64,
    pub message: Message,
}

impl Frame {
    pub fn new(session_id: Uuid, seq: u64, message: Message) -> Self {
        Self {
            session_id,
            seq,
            message,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Message {
    Hello(ProtocolHello),
    Heartbeat { monotonic_ms: u64 },
    HostState(HostStateEvent),
    ClientControl(ClientControlEvent),
    Input(InputEvent),
    InputReset,
}

impl Message {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Hello(_) => "hello",
            Self::Heartbeat { .. } => "heartbeat",
            Self::HostState(_) => "host_state",
            Self::ClientControl(_) => "client_control",
            Self::Input(event) => event.label(),
            Self::InputReset => "input_reset",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProtocolHello {
    pub protocol_version: u16,
    pub role: String,
    pub device_name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HostStateEvent {
    pub remote_active: bool,
    pub reason: String,
    #[serde(default)]
    pub entry_x_ratio: Option<f64>,
    #[serde(default)]
    pub entry_y_ratio: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ClientControlEvent {
    ReleaseHost {
        reason: String,
        #[serde(default)]
        entry_x_ratio: Option<f64>,
        #[serde(default)]
        entry_y_ratio: Option<f64>,
    },
    UdpMotionReady,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum InputEvent {
    MouseMotion {
        dx: i32,
        dy: i32,
    },
    MouseWheel {
        dx: i32,
        dy: i32,
    },
    MouseButton {
        button: MouseButton,
        state: KeyState,
    },
    Key {
        key: KeyCode,
        state: KeyState,
    },
    Modifier {
        modifier: Modifier,
        state: KeyState,
    },
}

impl InputEvent {
    pub fn label(&self) -> &'static str {
        match self {
            Self::MouseMotion { .. } => "mouse_motion",
            Self::MouseWheel { .. } => "mouse_wheel",
            Self::MouseButton { .. } => "mouse_button",
            Self::Key { .. } => "key",
            Self::Modifier { .. } => "modifier",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum KeyState {
    Down,
    Up,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum Modifier {
    Control,
    Alt,
    Super,
    Shift,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum KeyCode {
    Backslash,
    Escape,
    Space,
    Enter,
    Tab,
    Usb(u16),
    Other(u32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MotionDatagram {
    pub seq: u64,
    pub dx: i32,
    pub dy: i32,
}

impl MotionDatagram {
    #[cfg_attr(not(any(test, windows)), allow(dead_code))]
    pub fn encode(self) -> [u8; MOTION_DATAGRAM_LEN] {
        let mut out = [0_u8; MOTION_DATAGRAM_LEN];
        out[0..4].copy_from_slice(MOTION_DATAGRAM_MAGIC);
        out[4] = MOTION_DATAGRAM_TYPE_MOUSE_MOTION;
        out[8..16].copy_from_slice(&self.seq.to_le_bytes());
        out[16..20].copy_from_slice(&self.dx.to_le_bytes());
        out[20..24].copy_from_slice(&self.dy.to_le_bytes());
        out
    }

    pub fn decode(input: &[u8]) -> Option<Self> {
        if input.len() != MOTION_DATAGRAM_LEN {
            return None;
        }
        if &input[0..4] != MOTION_DATAGRAM_MAGIC {
            return None;
        }
        if input[4] != MOTION_DATAGRAM_TYPE_MOUSE_MOTION {
            return None;
        }

        let seq = u64::from_le_bytes(input[8..16].try_into().ok()?);
        let dx = i32::from_le_bytes(input[16..20].try_into().ok()?);
        let dy = i32::from_le_bytes(input[20..24].try_into().ok()?);
        Some(Self { seq, dx, dy })
    }
}

#[cfg(test)]
mod motion_tests {
    use super::*;

    #[test]
    fn motion_datagram_round_trips() {
        let packet = MotionDatagram {
            seq: 42,
            dx: -17,
            dy: 23,
        };
        assert_eq!(MotionDatagram::decode(&packet.encode()), Some(packet));
    }

    #[test]
    fn motion_datagram_rejects_bad_input() {
        assert_eq!(MotionDatagram::decode(&[]), None);
        let mut packet = MotionDatagram {
            seq: 1,
            dx: 2,
            dy: 3,
        }
        .encode();
        packet[0] = b'X';
        assert_eq!(MotionDatagram::decode(&packet), None);
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(windows, allow(dead_code))]
pub enum MacModifierPolicy {
    /// PC-keyboard physical order: Alt -> Command, Super -> Option.
    SwapAltSuper,
    /// Semantic mapping for Mac-like labels: Super -> Command, Alt -> Option.
    Native,
}

#[cfg_attr(windows, allow(dead_code))]
impl MacModifierPolicy {
    pub fn from_env() -> Self {
        match std::env::var("SOFTKVM_MAC_MODIFIER_POLICY") {
            Ok(value) if matches!(value.as_str(), "swap" | "swap-alt-super" | "pc") => {
                Self::SwapAltSuper
            }
            Ok(value) if matches!(value.as_str(), "native" | "mac" | "semantic") => Self::Native,
            _ => Self::SwapAltSuper,
        }
    }

    pub fn map(self, modifier: Modifier) -> MacModifier {
        match (self, modifier) {
            (Self::SwapAltSuper, Modifier::Alt) => MacModifier::Command,
            (Self::SwapAltSuper, Modifier::Super) => MacModifier::Option,
            (_, Modifier::Alt) => MacModifier::Option,
            (_, Modifier::Super) => MacModifier::Command,
            (_, Modifier::Control) => MacModifier::Control,
            (_, Modifier::Shift) => MacModifier::Shift,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(windows, allow(dead_code))]
pub enum MacModifier {
    Command,
    Option,
    Control,
    Shift,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_alt_super_policy_matches_windows_muscle_memory() {
        assert_eq!(
            MacModifierPolicy::SwapAltSuper.map(Modifier::Alt),
            MacModifier::Command
        );
        assert_eq!(
            MacModifierPolicy::SwapAltSuper.map(Modifier::Super),
            MacModifier::Option
        );
    }

    #[test]
    fn native_policy_matches_mac_semantics() {
        assert_eq!(
            MacModifierPolicy::Native.map(Modifier::Super),
            MacModifier::Command
        );
        assert_eq!(
            MacModifierPolicy::Native.map(Modifier::Alt),
            MacModifier::Option
        );
    }
}
