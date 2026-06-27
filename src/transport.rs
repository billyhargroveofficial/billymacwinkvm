use crate::protocol::Frame;
use anyhow::{Context, Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAX_FRAME_BYTES: usize = 1024 * 1024;

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
        self.stream
            .write_all(&(body.len() as u32).to_le_bytes())
            .await
            .context("write frame length")?;
        self.stream
            .write_all(&body)
            .await
            .context("write frame body")?;
        self.stream.flush().await.context("flush frame")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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
}
