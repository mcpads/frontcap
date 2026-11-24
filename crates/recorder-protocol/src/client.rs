//! A control server's connection to one recorder. One call at a time per client.
use crate::frame::{FrameError, read_frame, write_frame};
use crate::{Answer, Call, ClientFrame, ErrorReply, Reply, ServerFrame};
use std::path::Path;
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("the recorder closed the connection")]
    Closed,
    #[error("answer {got} does not match call {expected}")]
    OutOfOrder { expected: u64, got: u64 },
    #[error("{}: {} ({})", .0.message, .0.recovery, serde_json::to_string(&.0.code).unwrap_or_default())]
    Recorder(ErrorReply),
}

pub struct RecorderClient {
    read: BufReader<OwnedReadHalf>,
    write: OwnedWriteHalf,
    next: u64,
}

impl RecorderClient {
    /// Connects and requests the recorder's launch identity.
    pub async fn connect(socket: &Path) -> Result<(RecorderClient, Reply), ClientError> {
        let stream = UnixStream::connect(socket).await.map_err(FrameError::from)?;
        let (r, w) = stream.into_split();
        let mut c = RecorderClient { read: BufReader::new(r), write: w, next: 1 };
        let hello = c.call(Call::Hello).await?;
        Ok((c, hello))
    }

    /// Sends a call and returns without reading its answer, as a controller that dies right after sending.
    pub async fn send_without_waiting(&mut self, call: Call) -> Result<(), ClientError> {
        let id = self.next;
        self.next += 1;
        write_frame(&mut self.write, &ClientFrame { id, call }).await?;
        Ok(())
    }

    pub async fn call(&mut self, call: Call) -> Result<Reply, ClientError> {
        let id = self.next;
        self.next += 1;
        write_frame(&mut self.write, &ClientFrame { id, call }).await?;
        let frame: ServerFrame = read_frame(&mut self.read).await?.ok_or(ClientError::Closed)?;
        if frame.id != id {
            return Err(ClientError::OutOfOrder { expected: id, got: frame.id });
        }
        match frame.answer {
            Answer::Ok(reply) => Ok(reply),
            Answer::Err(e) => Err(ClientError::Recorder(e)),
        }
    }
}
