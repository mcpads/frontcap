/// An in-memory sink that fails once more than `limit` bytes have been written in total.
pub struct FailingSink {
    pub data: Vec<u8>,
    limit: usize,
}

impl FailingSink {
    pub fn new(limit: usize) -> FailingSink {
        FailingSink { data: Vec::new(), limit }
    }
}

impl super::Sink for FailingSink {
    fn write_all_bytes(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let room = self.limit.saturating_sub(self.data.len());
        self.data.extend_from_slice(&bytes[..room.min(bytes.len())]);
        if bytes.len() > room {
            return Err(std::io::Error::other("disk full"));
        }
        Ok(())
    }

    fn truncate_to(&mut self, len: u64) -> std::io::Result<()> {
        self.data.truncate(len as usize);
        Ok(())
    }

    fn sync(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
