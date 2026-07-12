use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 0;
pub const MOTION_DATAGRAM_LEN: usize = 24;
/// SKM2 appends a 32-bit sender monotonic-microsecond stamp used for
/// delay-variation tracing. Receivers accept both formats; the sender emits
/// SKM2 only while SOFTKVM_TRACE is enabled, so untraced runs are unchanged.
pub const MOTION_DATAGRAM_V2_LEN: usize = 32;
pub const MOTION_DATAGRAM_MAX_LEN: usize = MOTION_DATAGRAM_V2_LEN;
const MOTION_DATAGRAM_MAGIC: &[u8; 4] = b"SKM1";
const MOTION_DATAGRAM_MAGIC_V2: &[u8; 4] = b"SKM2";
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
    /// Sender monotonic clock, low 32 bits of microseconds. 0 when unknown
    /// (SKM1 sender or tracing disabled). Wraps every ~71.6 minutes; the
    /// analyzer subtracts wrap-aware.
    pub t_send_us: u32,
}

impl MotionDatagram {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(seq: u64, dx: i32, dy: i32) -> Self {
        Self {
            seq,
            dx,
            dy,
            t_send_us: 0,
        }
    }

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

    #[cfg_attr(not(any(test, windows)), allow(dead_code))]
    pub fn encode_v2(self) -> [u8; MOTION_DATAGRAM_V2_LEN] {
        let mut out = [0_u8; MOTION_DATAGRAM_V2_LEN];
        out[0..4].copy_from_slice(MOTION_DATAGRAM_MAGIC_V2);
        out[4] = MOTION_DATAGRAM_TYPE_MOUSE_MOTION;
        out[8..16].copy_from_slice(&self.seq.to_le_bytes());
        out[16..20].copy_from_slice(&self.dx.to_le_bytes());
        out[20..24].copy_from_slice(&self.dy.to_le_bytes());
        out[24..28].copy_from_slice(&self.t_send_us.to_le_bytes());
        out
    }

    pub fn decode(input: &[u8]) -> Option<Self> {
        let t_send_us = match input.len() {
            MOTION_DATAGRAM_LEN => {
                if &input[0..4] != MOTION_DATAGRAM_MAGIC {
                    return None;
                }
                0
            }
            MOTION_DATAGRAM_V2_LEN => {
                if &input[0..4] != MOTION_DATAGRAM_MAGIC_V2 {
                    return None;
                }
                u32::from_le_bytes(input[24..28].try_into().ok()?)
            }
            _ => return None,
        };
        if input[4] != MOTION_DATAGRAM_TYPE_MOUSE_MOTION {
            return None;
        }

        let seq = u64::from_le_bytes(input[8..16].try_into().ok()?);
        let dx = i32::from_le_bytes(input[16..20].try_into().ok()?);
        let dy = i32::from_le_bytes(input[20..24].try_into().ok()?);
        Some(Self {
            seq,
            dx,
            dy,
            t_send_us,
        })
    }
}

#[cfg(test)]
mod motion_tests {
    use super::*;

    #[test]
    fn motion_datagram_round_trips() {
        let packet = MotionDatagram::new(42, -17, 23);
        assert_eq!(MotionDatagram::decode(&packet.encode()), Some(packet));
    }

    #[test]
    fn motion_datagram_v2_round_trips_with_send_stamp() {
        let packet = MotionDatagram {
            seq: 42,
            dx: -17,
            dy: 23,
            t_send_us: 0xdead_beef,
        };
        assert_eq!(MotionDatagram::decode(&packet.encode_v2()), Some(packet));
    }

    #[test]
    fn motion_datagram_v1_decodes_without_send_stamp() {
        let packet = MotionDatagram {
            seq: 7,
            dx: 1,
            dy: 2,
            t_send_us: 999,
        };
        let decoded = MotionDatagram::decode(&packet.encode()).expect("v1 decode");
        assert_eq!(decoded.t_send_us, 0);
        assert_eq!((decoded.seq, decoded.dx, decoded.dy), (7, 1, 2));
    }

    #[test]
    fn motion_datagram_rejects_bad_input() {
        assert_eq!(MotionDatagram::decode(&[]), None);
        let mut packet = MotionDatagram::new(1, 2, 3).encode();
        packet[0] = b'X';
        assert_eq!(MotionDatagram::decode(&packet), None);
        let mut packet_v2 = MotionDatagram::new(1, 2, 3).encode_v2();
        packet_v2[3] = b'9';
        assert_eq!(MotionDatagram::decode(&packet_v2), None);
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
