mod cli;
mod hid;
mod platform;
mod protocol;
mod transport;

use anyhow::{Context, Result, bail};
use clap::Parser;
use cli::{Cli, Command, SinkKind};
use hid::{HidSink, KarabinerProbe, LogSink};
use protocol::{
    ClientControlEvent, Frame, InputEvent, KeyCode, KeyState, Message, Modifier, MouseButton,
    ProtocolHello,
};
use std::collections::HashSet;
use tokio::net::TcpStream;
use tokio::time::{Duration, sleep};
use tracing::{info, warn};
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    install_signal_guards();

    let cli = Cli::parse();

    match cli.command {
        Command::GenPsk => gen_psk(),
        Command::MacHidProbe => mac_hid_probe(),
        Command::MacHidSmoke => mac_hid_smoke().await,
        Command::MacKeySmoke => mac_key_smoke().await,
        Command::Client { listen, sink } => run_client(listen, sink).await,
        Command::Probe { peer } => run_probe(peer).await,
        Command::Host { peer, layout } => run_host(peer, layout).await,
    }
}

fn gen_psk() -> Result<()> {
    let bytes: [u8; 32] = rand::random();
    println!("{}", hex::encode(bytes));
    Ok(())
}

fn mac_hid_probe() -> Result<()> {
    let probe = KarabinerProbe::detect();
    println!("{}", serde_json::to_string_pretty(&probe)?);

    if !probe.ready {
        bail!(
            "Karabiner VirtualHID is not ready. Install/approve the system extension, then rerun this probe."
        );
    }

    Ok(())
}

async fn mac_hid_smoke() -> Result<()> {
    let mut sink = hid::karabiner_sink_async().await?;
    for _ in 0..30 {
        sink.apply(InputEvent::MouseMotion { dx: 8, dy: 0 }).await?;
        sleep(Duration::from_millis(5)).await;
    }
    for _ in 0..30 {
        sink.apply(InputEvent::MouseMotion { dx: -8, dy: 0 })
            .await?;
        sleep(Duration::from_millis(5)).await;
    }
    sink.reset().await?;
    Ok(())
}

async fn mac_key_smoke() -> Result<()> {
    let mut sink = hid::karabiner_sink_async().await?;
    info!("typing five synthetic 'a' key presses through Karabiner VirtualHID");
    for _ in 0..5 {
        sink.apply(InputEvent::Key {
            key: KeyCode::Usb(0x04),
            state: KeyState::Down,
        })
        .await?;
        sleep(Duration::from_millis(90)).await;
        sink.apply(InputEvent::Key {
            key: KeyCode::Usb(0x04),
            state: KeyState::Up,
        })
        .await?;
        sleep(Duration::from_millis(70)).await;
    }
    sink.reset().await?;
    Ok(())
}

async fn run_client(listen: String, sink: SinkKind) -> Result<()> {
    let mut hid_sink = make_sink(sink).await?;

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    info!(%listen, "softkvm client listening");

    loop {
        let (stream, addr) = listener.accept().await?;
        info!(%addr, "accepted host");
        if let Err(err) = stream.set_nodelay(true) {
            warn!(?err, "failed to set TCP_NODELAY on accepted host stream");
        }

        let (read_half, write_half) = stream.into_split();
        let mut reader = transport::FrameReader::new(read_half);
        let mut writer = transport::FrameWriter::new(write_half);
        let mut client_state = ClientRuntimeState::default();
        let client_session_id = Uuid::new_v4();
        let mut client_seq = 1_u64;
        let mut saw_frame = false;
        let mut touched_input = false;

        while let Some(frame) = reader.read_frame().await? {
            saw_frame = true;
            match frame.message {
                Message::Hello(hello) => {
                    info!(?hello, "host hello");
                }
                Message::HostState(state) => {
                    info!(?state, "host state");
                    client_state.remote_active = state.remote_active;
                    client_state.reset_keys();
                    if state.remote_active {
                        position_mac_cursor(state.entry_x_ratio, state.entry_y_ratio);
                    } else {
                        reset_input(&mut hid_sink, sink).await;
                    }
                }
                Message::ClientControl(control) => {
                    info!(?control, "client control from peer ignored");
                }
                Message::Input(event) => {
                    touched_input = true;
                    if client_state.should_release_for_hotkey(&event) {
                        client_state.remote_active = false;
                        reset_input(&mut hid_sink, sink).await;
                        if let Err(err) = writer
                            .write_frame(Frame::new(
                                client_session_id,
                                client_seq,
                                Message::ClientControl(ClientControlEvent::ReleaseHost {
                                    reason: "remote hotkey Ctrl+Alt+\\".to_owned(),
                                    entry_x_ratio: Some(0.5),
                                    entry_y_ratio: Some(0.5),
                                }),
                            ))
                            .await
                        {
                            warn!(?err, "failed to request host release from hotkey");
                        } else {
                            client_seq += 1;
                            info!("requested Windows host release from remote hotkey");
                        }
                        continue;
                    }
                    let check_right_edge = client_state.should_check_right_edge_release(&event);
                    apply_input(&mut hid_sink, sink, event).await;
                    if check_right_edge && mac_cursor_at_right_edge() {
                        client_state.remote_active = false;
                        reset_input(&mut hid_sink, sink).await;
                        let (_, y_ratio) = mac_cursor_ratios().unwrap_or((0.0, 0.5));
                        if let Err(err) = writer
                            .write_frame(Frame::new(
                                client_session_id,
                                client_seq,
                                Message::ClientControl(ClientControlEvent::ReleaseHost {
                                    reason: "mac right edge".to_owned(),
                                    entry_x_ratio: Some(0.02),
                                    entry_y_ratio: Some(y_ratio),
                                }),
                            ))
                            .await
                        {
                            warn!(?err, "failed to request host release");
                        } else {
                            client_seq += 1;
                            info!("requested Windows host release from macOS right edge");
                        }
                    }
                }
                Message::InputReset => {
                    touched_input = true;
                    client_state.reset_keys();
                    reset_input(&mut hid_sink, sink).await;
                }
                Message::Heartbeat { monotonic_ms } => {
                    info!(monotonic_ms, "heartbeat");
                }
            }
        }

        if saw_frame {
            warn!(%addr, "host disconnected");
        } else {
            info!(%addr, "empty tcp probe disconnected");
        }

        if touched_input {
            reset_input(&mut hid_sink, sink).await;
        }
    }
}

#[derive(Default)]
struct ClientRuntimeState {
    remote_active: bool,
    active_modifiers: HashSet<Modifier>,
}

impl ClientRuntimeState {
    fn reset_keys(&mut self) {
        self.active_modifiers.clear();
    }

    fn should_release_for_hotkey(&mut self, event: &InputEvent) -> bool {
        match event {
            InputEvent::Modifier { modifier, state } => {
                match state {
                    KeyState::Down => {
                        self.active_modifiers.insert(*modifier);
                    }
                    KeyState::Up => {
                        self.active_modifiers.remove(modifier);
                    }
                }
                false
            }
            InputEvent::Key {
                key: KeyCode::Backslash,
                state: KeyState::Down,
            } if self.remote_active => {
                self.active_modifiers.contains(&Modifier::Control)
                    && (self.active_modifiers.contains(&Modifier::Alt)
                        || self.active_modifiers.contains(&Modifier::Super))
            }
            _ => false,
        }
    }

    fn should_check_right_edge_release(&self, event: &InputEvent) -> bool {
        matches!(event, InputEvent::MouseMotion { dx, .. } if self.remote_active && *dx > 0)
    }
}

async fn make_sink(sink: SinkKind) -> Result<Box<dyn HidSink>> {
    match sink {
        SinkKind::Log => Ok(Box::new(LogSink::default())),
        SinkKind::Karabiner => hid::karabiner_sink_async().await,
    }
}

#[cfg(target_os = "macos")]
fn mac_cursor_at_right_edge() -> bool {
    const EDGE_TRIGGER_PX: f64 = 8.0;

    match unsafe { mac_cursor_and_main_display_bounds() } {
        Some((cursor, bounds)) => cursor.x >= bounds.origin.x + bounds.size.width - EDGE_TRIGGER_PX,
        None => false,
    }
}

#[cfg(target_os = "macos")]
fn mac_cursor_ratios() -> Option<(f64, f64)> {
    let (cursor, bounds) = unsafe { mac_cursor_and_main_display_bounds()? };
    let x = ((cursor.x - bounds.origin.x) / bounds.size.width).clamp(0.0, 1.0);
    let y = ((cursor.y - bounds.origin.y) / bounds.size.height).clamp(0.0, 1.0);
    Some((x, y))
}

#[cfg(not(target_os = "macos"))]
fn mac_cursor_ratios() -> Option<(f64, f64)> {
    None
}

#[cfg(target_os = "macos")]
fn position_mac_cursor(x_ratio: Option<f64>, y_ratio: Option<f64>) {
    let Some((x_ratio, y_ratio)) = x_ratio.zip(y_ratio) else {
        return;
    };
    let bounds = unsafe { CGDisplayBounds(CGMainDisplayID()) };
    let inset = 16.0;
    let x = bounds.origin.x + (bounds.size.width - inset * 2.0) * x_ratio.clamp(0.0, 1.0) + inset;
    let y = bounds.origin.y + (bounds.size.height - inset * 2.0) * y_ratio.clamp(0.0, 1.0) + inset;
    unsafe { CGWarpMouseCursorPosition(CGPoint { x, y }) };
}

#[cfg(not(target_os = "macos"))]
fn position_mac_cursor(_x_ratio: Option<f64>, _y_ratio: Option<f64>) {}

#[cfg(not(target_os = "macos"))]
fn mac_cursor_at_right_edge() -> bool {
    false
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
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGEventCreate(source: *const std::ffi::c_void) -> *mut std::ffi::c_void;
    fn CGEventGetLocation(event: *mut std::ffi::c_void) -> CGPoint;
    fn CGWarpMouseCursorPosition(new_cursor_position: CGPoint) -> i32;
    fn CFRelease(cf: *const std::ffi::c_void);
}

#[cfg(target_os = "macos")]
unsafe fn mac_cursor_and_main_display_bounds() -> Option<(CGPoint, CGRect)> {
    let event = unsafe { CGEventCreate(std::ptr::null()) };
    if event.is_null() {
        return None;
    }
    let cursor = unsafe { CGEventGetLocation(event) };
    unsafe { CFRelease(event.cast_const()) };
    let bounds = unsafe { CGDisplayBounds(CGMainDisplayID()) };
    Some((cursor, bounds))
}

async fn apply_input(hid_sink: &mut Box<dyn HidSink>, sink: SinkKind, event: InputEvent) {
    if let Err(err) = hid_sink.apply(event.clone()).await {
        warn!(?err, "input sink failed; reconnecting");
        match make_sink(sink).await {
            Ok(mut replacement) => {
                if let Err(retry_err) = replacement.apply(event).await {
                    warn!(?retry_err, "input sink retry failed");
                }
                *hid_sink = replacement;
            }
            Err(reconnect_err) => {
                warn!(?reconnect_err, "input sink reconnect failed");
            }
        }
    }
}

async fn reset_input(hid_sink: &mut Box<dyn HidSink>, sink: SinkKind) {
    if let Err(err) = hid_sink.reset().await {
        warn!(?err, "input sink reset failed; reconnecting");
        match make_sink(sink).await {
            Ok(mut replacement) => {
                if let Err(retry_err) = replacement.reset().await {
                    warn!(?retry_err, "input sink reset retry failed");
                }
                *hid_sink = replacement;
            }
            Err(reconnect_err) => {
                warn!(?reconnect_err, "input sink reconnect failed");
            }
        }
    }
}

async fn run_probe(peer: String) -> Result<()> {
    let stream = TcpStream::connect(&peer)
        .await
        .with_context(|| format!("connect {peer}"))?;
    stream.set_nodelay(true).context("set TCP_NODELAY")?;
    let mut writer = transport::FrameWriter::new(stream);
    let session_id = Uuid::new_v4();

    writer
        .write_frame(Frame::new(
            session_id,
            1,
            Message::Hello(ProtocolHello {
                device_name: hostname(),
                role: "probe".to_owned(),
                protocol_version: protocol::PROTOCOL_VERSION,
            }),
        ))
        .await?;

    for i in 0..240_u32 {
        let dx = if i < 120 { 4 } else { -4 };
        writer
            .write_frame(Frame::new(
                session_id,
                2 + u64::from(i),
                Message::Input(InputEvent::MouseMotion { dx, dy: 0 }),
            ))
            .await?;
        tokio::task::yield_now().await;
    }

    writer
        .write_frame(Frame::new(
            session_id,
            300,
            Message::Input(InputEvent::MouseButton {
                button: MouseButton::Left,
                state: KeyState::Down,
            }),
        ))
        .await?;
    writer
        .write_frame(Frame::new(
            session_id,
            301,
            Message::Input(InputEvent::MouseButton {
                button: MouseButton::Left,
                state: KeyState::Up,
            }),
        ))
        .await?;
    writer
        .write_frame(Frame::new(session_id, 302, Message::InputReset))
        .await?;

    Ok(())
}

async fn run_host(_peer: String, _layout: String) -> Result<()> {
    #[cfg(windows)]
    {
        platform::windows::run_host(_peer, _layout).await
    }

    #[cfg(not(windows))]
    {
        bail!(
            "host capture is Windows-only for the first MVP; use `probe` from macOS for client testing"
        )
    }
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "softkvm".to_owned())
}

#[cfg(unix)]
fn install_signal_guards() {
    tokio::spawn(async {
        let Ok(mut sigquit) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::quit())
        else {
            return;
        };
        while sigquit.recv().await.is_some() {
            warn!("ignored SIGQUIT; remote Ctrl+\\ should be handled as a KVM hotkey");
        }
    });
}

#[cfg(not(unix))]
fn install_signal_guards() {}
