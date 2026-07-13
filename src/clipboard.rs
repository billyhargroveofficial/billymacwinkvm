//! Shared-clipboard support: change detection, echo suppression, size caps.
//!
//! Platform backends (NSPasteboard on macOS, Win32 clipboard on Windows) live
//! next to the rest of the platform code; this module holds what both sides
//! share. Sync is disabled with SOFTKVM_CLIPBOARD=0.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::protocol::ClipboardEvent;

/// Refuse to ship clipboard images larger than this many PNG bytes.
pub const MAX_IMAGE_PNG_BYTES: usize = 24 * 1024 * 1024;
/// Refuse to ship text larger than this many UTF-8 bytes.
pub const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;

/// Content applied to the local clipboard from the peer; the next local
/// change notification for identical content is an echo and must not be
/// re-sent.
static LAST_APPLIED_HASH: AtomicU64 = AtomicU64::new(0);
/// Content most recently sent to the peer; identical repeat changes are
/// suppressed too (some apps re-set the clipboard several times per copy).
static LAST_SENT_HASH: AtomicU64 = AtomicU64::new(0);

pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("SOFTKVM_CLIPBOARD")
            .map(|value| {
                !matches!(
                    value.to_ascii_lowercase().as_str(),
                    "0" | "false" | "off" | "no"
                )
            })
            .unwrap_or(true)
    })
}

pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // Reserve 0 as the "nothing yet" sentinel.
    hash.max(1)
}

pub fn event_hash(event: &ClipboardEvent) -> u64 {
    match event {
        ClipboardEvent::Text(text) => fnv1a64(text.as_bytes()),
        ClipboardEvent::ImagePng { base64 } => fnv1a64(base64.as_bytes()),
    }
}

/// Should a locally observed clipboard change be sent to the peer?
/// Records the content as sent when it answers yes.
pub fn should_send(event: &ClipboardEvent) -> bool {
    let hash = event_hash(event);
    if LAST_APPLIED_HASH.load(Ordering::Acquire) == hash {
        return false;
    }
    LAST_SENT_HASH.swap(hash, Ordering::AcqRel) != hash
}

/// Record content about to be written into the local clipboard from the
/// peer, so the resulting change notification is ignored.
pub fn note_applied(event: &ClipboardEvent) {
    LAST_APPLIED_HASH.store(event_hash(event), Ordering::Release);
}

pub fn within_limits(event: &ClipboardEvent) -> bool {
    match event {
        ClipboardEvent::Text(text) => text.len() <= MAX_TEXT_BYTES,
        // base64 of the PNG; 4/3 expansion over the byte cap.
        ClipboardEvent::ImagePng { base64 } => base64.len() <= MAX_IMAGE_PNG_BYTES / 3 * 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_of_applied_content_is_suppressed() {
        let event = ClipboardEvent::Text("hello".to_owned());
        note_applied(&event);
        assert!(!should_send(&event));

        let fresh = ClipboardEvent::Text("world".to_owned());
        assert!(should_send(&fresh));
        // Re-announcing identical content is suppressed as well.
        assert!(!should_send(&fresh));
    }

    #[test]
    fn hash_reserves_zero_sentinel() {
        assert_ne!(fnv1a64(b""), 0);
        assert_ne!(fnv1a64(b"\x00"), 0);
    }
}
