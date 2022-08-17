//! The event reader reads messages from the read side of the FreeSWITCH event socket.
use crate::{sequence, Content, Headers, Message, LF};
use anyhow::{bail, Result};
use std::mem;
use tokio::io::{AsyncRead, AsyncReadExt};

const BUFFER_SIZE: usize = 0x4000;

#[derive(Debug)]
pub struct EventReader<R: AsyncRead + Unpin> {
    reader: R,
    socket_buffer: [u8; BUFFER_SIZE],
    // The number of bytes already consumed from the read_buffer.
    read_consumed: usize,
    read_buffer: Vec<u8>,
}

impl<R: AsyncRead + Unpin> EventReader<R> {
    pub fn new(read: R) -> Self {
        Self {
            reader: read,
            socket_buffer: [0; BUFFER_SIZE],
            read_consumed: 0,
            read_buffer: Default::default(),
        }
    }

    pub async fn read_event(&mut self) -> Result<Message> {
        let headers = self.read_headers().await?;
        let content = {
            if let Some((ty, len)) = headers.content()? {
                let data = self.read_data(len).await?.to_vec();
                Some(Content::new(ty, data))
            } else {
                None
            }
        };

        Ok(Message { headers, content })
    }

    async fn read_headers(&mut self) -> Result<Headers> {
        let block = self.read_block().await?;
        Headers::parse(block)
    }

    /// Reads until the given bytes have been received. `len` may be `0` which immediately returns
    /// an empty slice.
    async fn read_data(&mut self, len: usize) -> Result<&[u8]> {
        self.drain_read_buffer();

        if len == 0 {
            // We _do_ support empty content.
            return Ok(&[]);
        }

        loop {
            if self.read_buffer.len() >= len {
                self.read_consumed = len;
                return Ok(&self.read_buffer[..len]);
            }

            self.read_more().await?;
        }
    }

    /// Reads until two LFs are seen. Returns the block without the two LFs.
    async fn read_block(&mut self) -> Result<&[u8]> {
        self.drain_read_buffer();
        let mut start_search = 0;

        if self.read_buffer.is_empty() {
            self.read_more().await?;
        }

        loop {
            debug_assert!(!self.read_buffer.is_empty());
            let search_range = &self.read_buffer[start_search..self.read_buffer.len()];
            let sequence = &[LF, LF];
            if let Some(pos) = sequence::find_first(search_range, sequence) {
                let block_size = start_search + pos;
                self.read_consumed = block_size + sequence.len();
                return Ok(&self.read_buffer[..block_size]);
            } else {
                // Start search after the last sequence that hasn't matched.
                start_search = self.read_buffer.len() - (sequence.len() - 1);
            }

            self.read_more().await?;
        }
    }

    /// Remove the consumed data from the read buffer.
    fn drain_read_buffer(&mut self) {
        self.read_buffer.drain(0..self.read_consumed);
        self.read_consumed = 0;
    }

    /// Read more bytes into the read buffer.
    async fn read_more(&mut self) -> Result<()> {
        let mut read_buffer = mem::take(&mut self.read_buffer);
        let more = self.read_some().await?;
        read_buffer.extend(more);
        self.read_buffer = read_buffer;
        Ok(())
    }

    /// Read one or more bytes.
    async fn read_some(&mut self) -> Result<&[u8]> {
        let size = self.reader.read(&mut self.socket_buffer[..]).await?;
        if size == 0 {
            bail!("Socket receiver closed by peer.")
        };
        Ok(&self.socket_buffer[0..size])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_test::io;

    #[tokio::test]
    async fn reading_block_lf_lf() {
        let mock = io::Builder::new().read(b"\n").read(b"\n").build();
        let mut reader = EventReader::new(mock);
        let read = reader.read_block().await.unwrap();
        assert_eq!(read, &[0u8; 0]);
    }

    #[tokio::test]
    async fn read_block_content_lf_lf() {
        let mock = io::Builder::new().read(b"xxxx\n").read(b"\n").build();
        let mut reader = EventReader::new(mock);
        let read = reader.read_block().await.unwrap();
        assert_eq!(read, b"xxxx");
    }

    #[tokio::test]
    async fn read_two_blocks() {
        let mock = io::Builder::new().read(b"a\n\n").read(b"b\n\n").build();
        let mut reader = EventReader::new(mock);
        let read = reader.read_block().await.unwrap();
        assert_eq!(read, b"a");
        let read = reader.read_block().await.unwrap();
        assert_eq!(read, b"b");
    }

    #[tokio::test]
    async fn read_two_blocks_combined() {
        let mock = io::Builder::new().read(b"a\n\nb\n\n").build();
        let mut reader = EventReader::new(mock);
        let read = reader.read_block().await.unwrap();
        assert_eq!(read, b"a");
        let read = reader.read_block().await.unwrap();
        assert_eq!(read, b"b");
    }

    #[tokio::test]
    async fn reader_fails_with_empty_read_result() {
        let mock = io::Builder::new().read(b"a\n\n").read(b"").build();
        let mut reader = EventReader::new(mock);
        let read = reader.read_block().await.unwrap();
        assert_eq!(read, b"a");
        let read = reader.read_block().await;
        assert!(read.is_err());
    }

    #[tokio::test]
    async fn read_data_spans_two_packets() {
        let mock = io::Builder::new().read(b"a").read(b"b").build();
        let mut reader = EventReader::new(mock);
        let read = reader.read_data(2).await.unwrap();
        assert_eq!(read, b"ab");
    }

    #[tokio::test]
    async fn read_data_reads_twice_from_one_packet() {
        let mock = io::Builder::new().read(b"ab").build();
        let mut reader = EventReader::new(mock);
        let read1 = reader.read_data(1).await.unwrap();
        assert_eq!(read1, b"a");
        let read2 = reader.read_data(1).await.unwrap();
        assert_eq!(read2, b"b");
    }
}
