use crate::protocol::Frame;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream, lookup_host};
use tokio::time::sleep;
use tracing::warn;

const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Local-port scan used when the OS ephemeral allocator is broken (outbound
/// connect/bind failing with WSAEADDRINUSE 10048, typically because Hyper-V/
/// WSL/WinNAT port-exclusion ranges swallowed the dynamic range 49152-65535).
/// Candidates live in 21309..=27772: below every default dynamic range, above
/// common service ports, and away from the softkvm port. Binding explicitly
/// bypasses the dynamic allocator entirely.
pub const EXPLICIT_BIND_PORT_ATTEMPTS: u16 = 64;

pub fn explicit_bind_candidate_port(index: u16) -> u16 {
    let offset = (std::process::id() % 101) as u16;
    21309 + offset + index * 101
}

fn explicit_bind_retryable(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::AddrInUse
            | io::ErrorKind::AddrNotAvailable
            | io::ErrorKind::PermissionDenied
    )
}

/// Connect with an explicitly bound local port, scanning candidates. Used as
/// a fallback when plain connect() cannot allocate an ephemeral port.
pub async fn connect_via_explicit_local_bind(peer: &str) -> io::Result<TcpStream> {
    let addr: SocketAddr = match peer.parse() {
        Ok(addr) => addr,
        Err(_) => lookup_host(peer).await?.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "peer resolved to nothing")
        })?,
    };

    let mut last_error = None;
    for index in 0..EXPLICIT_BIND_PORT_ATTEMPTS {
        let port = explicit_bind_candidate_port(index);
        let (socket, bind_addr): (TcpSocket, SocketAddr) = if addr.is_ipv4() {
            (TcpSocket::new_v4()?, (Ipv4Addr::UNSPECIFIED, port).into())
        } else {
            (TcpSocket::new_v6()?, (Ipv6Addr::UNSPECIFIED, port).into())
        };
        if let Err(err) = socket.bind(bind_addr) {
            last_error = Some(err);
            continue;
        }
        match socket.connect(addr).await {
            Ok(stream) => return Ok(stream),
            Err(err) if explicit_bind_retryable(err.kind()) => {
                last_error = Some(err);
                continue;
            }
            Err(err) => return Err(err),
        }
    }
    Err(last_error
        .unwrap_or_else(|| io::Error::new(io::ErrorKind::AddrInUse, "local bind scan exhausted")))
}

/// Bind a UDP socket on an ephemeral port, falling back to the explicit port
/// scan when the dynamic allocator is broken (same 10048 failure mode).
pub fn bind_udp_with_fallback() -> io::Result<std::net::UdpSocket> {
    match std::net::UdpSocket::bind("0.0.0.0:0") {
        Ok(socket) => Ok(socket),
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
            for index in 0..EXPLICIT_BIND_PORT_ATTEMPTS {
                let port = explicit_bind_candidate_port(index);
                if let Ok(socket) = std::net::UdpSocket::bind(("0.0.0.0", port)) {
                    warn!(
                        port,
                        "udp ephemeral allocation failed; bound explicit local port instead"
                    );
                    return Ok(socket);
                }
            }
            Err(err)
        }
        Err(err) => Err(err),
    }
}

pub async fn connect_tcp_with_retry(
    peer: &str,
    attempts: usize,
    retry_delay: Duration,
) -> Result<TcpStream> {
    let attempts = attempts.max(1);
    let mut last_error = None;

    for attempt in 1..=attempts {
        match TcpStream::connect(peer).await {
            Ok(stream) => {
                if attempt > 1 {
                    tracing::info!(%peer, attempt, "TCP connection recovered");
                }
                return Ok(stream);
            }
            Err(err) => {
                warn!(
                    %peer,
                    attempt,
                    attempts,
                    error = %err,
                    error_kind = ?err.kind(),
                    raw_os_error = ?err.raw_os_error(),
                    "TCP connect failed"
                );
                if err.kind() == io::ErrorKind::AddrInUse {
                    // Outbound connect() failing with WSAEADDRINUSE/EADDRINUSE
                    // means the OS could not allocate a local ephemeral port
                    // (excluded/shrunken dynamic range, TIME_WAIT exhaustion).
                    // Bypass the allocator by binding a local port explicitly.
                    match connect_via_explicit_local_bind(peer).await {
                        Ok(stream) => {
                            let local = stream
                                .local_addr()
                                .map(|addr| addr.to_string())
                                .unwrap_or_else(|_| "unknown".to_owned());
                            warn!(
                                %peer,
                                local,
                                "ephemeral port allocation is broken on this machine; \
                                 connected via explicit local bind. Run \
                                 `softkvm.exe win-port-doctor --peer <mac-ip>:49321` \
                                 to diagnose and fix the OS-level cause"
                            );
                            return Ok(stream);
                        }
                        Err(fallback_err) => {
                            if attempt == 1 {
                                warn!(
                                    error = %fallback_err,
                                    "explicit local bind fallback also failed; run \
                                     `softkvm.exe win-port-doctor` (or \
                                     scripts/diagnose-addrinuse.ps1) to check dynamicport, \
                                     excludedportrange, TIME_WAIT counts, and duplicate \
                                     softkvm processes"
                                );
                            }
                        }
                    }
                }
                last_error = Some(err);
                if attempt < attempts {
                    sleep(retry_delay).await;
                }
            }
        }
    }

    let err = last_error.expect("at least one TCP connection attempt");
    bail!(connect_error_message(peer, attempts, &err))
}

fn connect_error_message(peer: &str, attempts: usize, err: &io::Error) -> String {
    format!(
        "connect {peer} failed after {attempts} attempts: {err} (kind={:?}, raw_os_error={:?})",
        err.kind(),
        err.raw_os_error()
    )
}

pub struct FrameReader<R> {
    stream: R,
}

impl<R> FrameReader<R>
where
    R: AsyncRead + Unpin,
{
    pub fn new(stream: R) -> Self {
        Self { stream }
    }

    pub async fn read_frame(&mut self) -> Result<Option<Frame>> {
        let mut len = [0_u8; 4];
        match self.stream.read_exact(&mut len).await {
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
                ) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e).context("read frame length"),
        }

        let len = u32::from_le_bytes(len) as usize;
        if len > MAX_FRAME_BYTES {
            bail!("frame too large: {len}");
        }

        let mut body = vec![0_u8; len];
        self.stream
            .read_exact(&mut body)
            .await
            .context("read frame body")?;
        let frame = serde_json::from_slice(&body).context("decode frame")?;
        Ok(Some(frame))
    }
}

pub struct FrameWriter<W> {
    stream: W,
}

impl<W> FrameWriter<W>
where
    W: AsyncWrite + Unpin,
{
    pub fn new(stream: W) -> Self {
        Self { stream }
    }

    pub async fn write_frame(&mut self, frame: Frame) -> Result<()> {
        let body = serde_json::to_vec(&frame).context("encode frame")?;
        if body.len() > MAX_FRAME_BYTES {
            bail!("frame too large: {}", body.len());
        }

        let mut wire = Vec::with_capacity(4 + body.len());
        wire.extend_from_slice(&(body.len() as u32).to_le_bytes());
        wire.extend_from_slice(&body);
        self.stream.write_all(&wire).await.context("write frame")?;
        self.stream.flush().await.context("flush frame")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use tokio::net::{TcpListener, TcpStream};
    use uuid::Uuid;

    use super::*;
    use crate::protocol::{Message, PROTOCOL_VERSION, ProtocolHello};

    #[tokio::test]
    async fn round_trips_frame() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;

        let sender = tokio::spawn(async move {
            let stream = TcpStream::connect(addr).await?;
            let mut writer = FrameWriter::new(stream);
            writer
                .write_frame(Frame {
                    session_id: Uuid::nil(),
                    seq: 7,
                    message: Message::Hello(ProtocolHello {
                        protocol_version: PROTOCOL_VERSION,
                        role: "test-host".to_owned(),
                        device_name: "unit-test".to_owned(),
                    }),
                })
                .await
        });

        let (stream, _) = listener.accept().await?;
        let mut reader = FrameReader::new(stream);
        let frame = reader.read_frame().await?.expect("frame");

        assert_eq!(frame.seq, 7);
        match frame.message {
            Message::Hello(hello) => {
                assert_eq!(hello.role, "test-host");
                assert_eq!(hello.device_name, "unit-test");
            }
            other => panic!("unexpected message: {other:?}"),
        }

        sender.await??;
        Ok(())
    }

    #[tokio::test]
    async fn eof_is_clean_disconnect() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;

        let sender = tokio::spawn(async move {
            let stream = TcpStream::connect(addr).await?;
            drop(stream);
            Ok::<_, anyhow::Error>(())
        });

        let (stream, _) = listener.accept().await?;
        let mut reader = FrameReader::new(stream);

        assert!(reader.read_frame().await?.is_none());
        sender.await??;
        Ok(())
    }

    #[tokio::test]
    async fn explicit_local_bind_connect_reaches_listener() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let peer = addr.to_string();

        let (client, accepted) =
            tokio::join!(connect_via_explicit_local_bind(&peer), listener.accept());
        let client = client?;
        let (_server, _) = accepted?;

        let local_port = client.local_addr()?.port();
        let in_scan_range = (0..EXPLICIT_BIND_PORT_ATTEMPTS)
            .any(|index| explicit_bind_candidate_port(index) == local_port);
        assert!(in_scan_range, "local port {local_port} not from scan range");
        Ok(())
    }

    #[test]
    fn explicit_bind_candidates_stay_below_dynamic_range() {
        for index in 0..EXPLICIT_BIND_PORT_ATTEMPTS {
            let port = explicit_bind_candidate_port(index);
            assert!(
                (21309..28000).contains(&port),
                "candidate {port} out of range"
            );
        }
    }

    #[test]
    fn udp_fallback_binds() -> Result<()> {
        let socket = bind_udp_with_fallback()?;
        assert_ne!(socket.local_addr()?.port(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn retry_connector_reaches_listener() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let peer = addr.to_string();

        let connected = connect_tcp_with_retry(&peer, 3, Duration::from_millis(1));
        let (client, accepted) = tokio::join!(connected, listener.accept());

        let _client = client?;
        let (_server, _) = accepted?;
        Ok(())
    }

    #[test]
    fn connect_error_includes_os_details() {
        let error = io::Error::from_raw_os_error(61);
        let message = connect_error_message("192.0.2.1:49321", 8, &error);

        assert!(message.contains("192.0.2.1:49321"));
        assert!(message.contains("8 attempts"));
        assert!(message.contains("raw_os_error=Some(61)"));
    }
}
