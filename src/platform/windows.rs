use anyhow::{Result, bail};

pub async fn run_host(peer: String, layout: String) -> Result<()> {
    let _ = (peer, layout);
    bail!(
        "Windows host capture skeleton is documented in docs/windows-host.md; implement on a Windows build machine"
    )
}

// Target architecture for the first Windows host implementation:
//
// - Hidden/message-only HWND + Win32 message loop.
// - RegisterRawInputDevices for mouse (usage page 0x01, usage 0x02) and keyboard
//   (usage page 0x01, usage 0x06) with RIDEV_INPUTSINK | RIDEV_DEVNOTIFY.
// - Ctrl+Alt+\ hotkey via RegisterHotKey using VK_OEM_5, plus hook-side handling
//   while RemoteActive so the chord never leaks to Windows or macOS.
// - Edge detection via GetCursorPos and virtual desktop metrics:
//   SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN.
// - RemoteActive local suppression through WH_KEYBOARD_LL and WH_MOUSE_LL only.
// - Cursor parked/clipped at the left Windows edge with ClipCursor while Mac is
//   active; always release on disconnect, shutdown, panic path, and sleep.
// - For very high polling mice, graduate from per-WM_INPUT GetRawInputData to
//   GetRawInputBuffer if input backlog shows up in telemetry.
