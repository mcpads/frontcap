//! One canonical JSON object per line. Serialized JSON never contains a raw newline, so a line is a frame.
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME: usize = 16 << 20;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("frame exceeds {MAX_FRAME} bytes")]
    TooLarge,
    #[error("malformed frame: {0}")]
    Malformed(#[from] serde_json::Error),
}

pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, value: &T) -> Result<(), FrameError> {
    let mut line = frontcap_core::canonical::canonical_of(value).into_bytes();
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await?;
    Ok(())
}

/// The next frame, or `None` at a clean end of stream.
pub async fn read_frame<R: AsyncBufRead + Unpin, T: DeserializeOwned>(r: &mut R) -> Result<Option<T>, FrameError> {
    let mut line = Vec::new();
    loop {
        let buf = r.fill_buf().await?;
        if buf.is_empty() {
            return if line.is_empty() { Ok(None) } else { Err(FrameError::Io(std::io::ErrorKind::UnexpectedEof.into())) };
        }
        match buf.iter().position(|b| *b == b'\n') {
            Some(i) => {
                line.extend_from_slice(&buf[..i]);
                r.consume(i + 1);
                break;
            }
            None => {
                let n = buf.len();
                line.extend_from_slice(buf);
                r.consume(n);
            }
        }
        if line.len() > MAX_FRAME {
            return Err(FrameError::TooLarge);
        }
    }
    if line.len() > MAX_FRAME {
        return Err(FrameError::TooLarge);
    }
    Ok(Some(serde_json::from_slice(&line)?))
}

#[cfg(test)]
#[path = "frame_tests.rs"]
mod tests;
