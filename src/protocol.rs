use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 0;

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
    Input(InputEvent),
    InputReset,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProtocolHello {
    pub protocol_version: u16,
    pub role: String,
    pub device_name: String,
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
    Other(u32),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum MacModifierPolicy {
    /// Windows-like muscle memory for remote Mac: Alt -> Command, Super -> Option.
    SwapAltSuper,
    /// Raw platform-ish mapping: Super -> Command, Alt -> Option.
    Native,
}

impl MacModifierPolicy {
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
}
