use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::protocol::Frame;

const MAX_FRAME_BYTES: usize = 1024 * 1024;

pub struct FrameReader {
    stream: TcpStream,
}

impl FrameReader {
    pub fn new(stream: TcpStream) -> Self {
        Self { stream }
    }

    pub async fn read_frame(&mut self) -> Result<Option<Frame>> {
        let mut len = [0_u8; 4];
        match self.stream.read_exact(&mut len).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
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

pub struct FrameWriter {
    stream: TcpStream,
}

impl FrameWriter {
    pub fn new(stream: TcpStream) -> Self {
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
