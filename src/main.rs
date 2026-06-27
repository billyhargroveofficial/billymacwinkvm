mod cli;
mod hid;
mod platform;
mod protocol;
mod transport;

use anyhow::{Context, Result, bail};
use clap::Parser;
use cli::{Cli, Command, SinkKind};
use hid::{HidSink, KarabinerProbe, LogSink};
use protocol::{Frame, InputEvent, KeyState, Message, MouseButton, ProtocolHello};
use tokio::net::TcpStream;
use tokio::time::{Duration, sleep};
use tracing::{info, warn};
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::GenPsk => gen_psk(),
        Command::MacHidProbe => mac_hid_probe(),
        Command::MacHidSmoke => mac_hid_smoke().await,
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

async fn run_client(listen: String, sink: SinkKind) -> Result<()> {
    let mut sink: Box<dyn HidSink> = match sink {
        SinkKind::Log => Box::new(LogSink::default()),
        SinkKind::Karabiner => hid::karabiner_sink_async().await?,
    };

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    info!(%listen, "softkvm client listening");

    loop {
        let (stream, addr) = listener.accept().await?;
        info!(%addr, "accepted host");

        let mut reader = transport::FrameReader::new(stream);
        while let Some(frame) = reader.read_frame().await? {
            match frame.message {
                Message::Hello(hello) => {
                    info!(?hello, "host hello");
                }
                Message::Input(event) => {
                    sink.apply(event).await?;
                }
                Message::InputReset => {
                    sink.reset().await?;
                }
                Message::Heartbeat { monotonic_ms } => {
                    info!(monotonic_ms, "heartbeat");
                }
            }
        }

        warn!(%addr, "host disconnected");
        sink.reset().await?;
    }
}

async fn run_probe(peer: String) -> Result<()> {
    let stream = TcpStream::connect(&peer)
        .await
        .with_context(|| format!("connect {peer}"))?;
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
        sleep(Duration::from_millis(4)).await;
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
