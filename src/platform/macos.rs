use tracing::{info, warn};

const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;

pub fn prepare_low_latency_thread(label: &'static str) {
    let result = unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
    if result == 0 {
        info!(label, "raised macOS thread QoS to user-interactive");
    } else {
        warn!(label, result, "failed to raise macOS thread QoS");
    }
}

unsafe extern "C" {
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
}

// ---------------------------------------------------------------------------
// Clipboard (NSPasteboard)
// ---------------------------------------------------------------------------

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use objc2::{AnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardTypePNG,
    NSPasteboardTypeString, NSPasteboardTypeTIFF,
};
use objc2_foundation::{NSData, NSDictionary, NSString};

use crate::clipboard;
use crate::protocol::ClipboardEvent;

/// Read the current pasteboard as a sync event: image (PNG preferred,
/// TIFF converted) wins over text so screenshots copy correctly.
fn read_pasteboard_event(pasteboard: &NSPasteboard) -> Option<ClipboardEvent> {
    unsafe {
        if let Some(png) = pasteboard.dataForType(NSPasteboardTypePNG) {
            return Some(ClipboardEvent::ImagePng {
                base64: BASE64.encode(png.to_vec()),
            });
        }
        if let Some(tiff) = pasteboard.dataForType(NSPasteboardTypeTIFF)
            && let Some(png) = tiff_to_png(&tiff)
        {
            return Some(ClipboardEvent::ImagePng {
                base64: BASE64.encode(png),
            });
        }
        if let Some(text) = pasteboard.stringForType(NSPasteboardTypeString) {
            return Some(ClipboardEvent::Text(text.to_string()));
        }
    }
    None
}

fn tiff_to_png(tiff: &NSData) -> Option<Vec<u8>> {
    unsafe {
        let rep = NSBitmapImageRep::initWithData(NSBitmapImageRep::alloc(), tiff)?;
        let png = rep
            .representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())?;
        Some(png.to_vec())
    }
}

fn png_to_tiff(png: &NSData) -> Option<objc2::rc::Retained<NSData>> {
    let rep = NSBitmapImageRep::initWithData(NSBitmapImageRep::alloc(), png)?;
    rep.TIFFRepresentation()
}

/// Apply a peer clipboard event to the local pasteboard.
pub fn apply_clipboard_event(event: &ClipboardEvent) {
    clipboard::note_applied(event);
    let pasteboard = NSPasteboard::generalPasteboard();
    match event {
        ClipboardEvent::Text(text) => unsafe {
            pasteboard.clearContents();
            if !pasteboard.setString_forType(&NSString::from_str(text), NSPasteboardTypeString) {
                warn!("failed to set macOS pasteboard text");
            }
        },
        ClipboardEvent::ImagePng { base64 } => {
            let Ok(bytes) = BASE64.decode(base64) else {
                warn!("peer clipboard image was not valid base64");
                return;
            };
            unsafe {
                let png = NSData::with_bytes(&bytes);
                let tiff = png_to_tiff(&png);
                pasteboard.clearContents();
                let mut ok = pasteboard.setData_forType(Some(&png), NSPasteboardTypePNG);
                if let Some(tiff) = tiff {
                    ok |= pasteboard.setData_forType(Some(&tiff), NSPasteboardTypeTIFF);
                }
                if !ok {
                    warn!("failed to set macOS pasteboard image");
                }
            }
        }
    }
}

/// Poll the pasteboard for local changes and forward them to the peer.
/// NSPasteboard has no change notification; polling changeCount is the
/// standard approach and costs microseconds per tick.
pub fn spawn_clipboard_watcher(tx: tokio::sync::mpsc::UnboundedSender<ClipboardEvent>) {
    if !clipboard::enabled() {
        info!("clipboard sync disabled (SOFTKVM_CLIPBOARD=0)");
        return;
    }
    std::thread::Builder::new()
        .name("softkvm-clipboard".to_owned())
        .spawn(move || {
            let pasteboard = NSPasteboard::generalPasteboard();
            let mut last_count = pasteboard.changeCount();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(500));
                let count = pasteboard.changeCount();
                if count == last_count {
                    continue;
                }
                last_count = count;
                let Some(event) = read_pasteboard_event(&pasteboard) else {
                    continue;
                };
                if !clipboard::within_limits(&event) {
                    info!(kind = event.label(), "clipboard content too large to sync");
                    continue;
                }
                if !clipboard::should_send(&event) {
                    continue;
                }
                info!(kind = event.label(), "sending local clipboard to peer");
                if tx.send(event).is_err() {
                    return;
                }
            }
        })
        .map(|_| ())
        .unwrap_or_else(|err| warn!(?err, "failed to spawn clipboard watcher"));
}

// ---------------------------------------------------------------------------
// Menu-bar tray
// ---------------------------------------------------------------------------

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::sel;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSMenu, NSMenuItem, NSStatusBar,
    NSVariableStatusItemLength,
};

pub fn tray_enabled() -> bool {
    std::env::var("SOFTKVM_TRAY")
        .map(|value| {
            !matches!(
                value.to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        })
        .unwrap_or(true)
}

/// Own the main thread forever: a menu-bar status item with a Quit entry.
/// Everything else (tokio, motion threads) must already run elsewhere.
pub fn run_menu_bar_tray(status_line: &str) -> ! {
    let Some(mtm) = MainThreadMarker::new() else {
        warn!("tray requested off the main thread; running headless instead");
        loop {
            std::thread::park();
        }
    };

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let status_bar = NSStatusBar::systemStatusBar();
    let item = status_bar.statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = item.button(mtm) {
        button.setTitle(&NSString::from_str("⌘⇄"));
    }

    let menu = NSMenu::new(mtm);
    let status = NSMenuItem::new(mtm);
    status.setTitle(&NSString::from_str(status_line));
    status.setEnabled(false);
    menu.addItem(&status);
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    let quit: Retained<NSMenuItem> = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Quit softkvm"),
            Some(sel!(terminate:)),
            &NSString::from_str("q"),
        )
    };
    menu.addItem(&quit);
    item.setMenu(Some(&menu));

    info!("menu-bar tray running");
    app.run();
    std::process::exit(0)
}
