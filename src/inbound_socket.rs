//! # FreeSWITCH inbound socket.
//!
//! ADR:
//! Initially we thought about creating a design that polls the events and the commands at the same
//! time, but it turned out that select! not simple to use on futures that are not cancellation safe
//! (which our event reader isn't, because of the intermediate buffers).
//!
//! To remedy that, a channel mpsc queue based approach was used. The event reader and the command
//! sender push both to the same queue which gets processed by a driver.
use crate::{
    event::{content_types, ApiResponse, AuthRequest, CommandReply, DisconnectNotice, ReplyText},
    query, sequence, Content, FromMessage, Headers, Message, LF,
};
use anyhow::{anyhow, bail, Context, Result};
use log::{debug, error, trace};
use std::{
    collections::{HashMap, VecDeque},
    mem, str,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpStream, ToSocketAddrs,
    },
    spawn,
    sync::{
        mpsc::{self, Sender},
        oneshot,
    },
    task::JoinHandle,
};
use uuid::Uuid;

const BUFFER_SIZE: usize = 0x4000;

#[derive(Debug)]
pub struct InboundSocket {
    driver_sender: mpsc::Sender<DriverMessage>,
    reader: JoinHandle<Result<()>>,
    driver: JoinHandle<Result<Option<DisconnectNotice>>>,
}

impl Drop for InboundSocket {
    fn drop(&mut self) {
        self.reader.abort();
        self.driver.abort();
    }
}

const MAX_QUEUE_SIZE: usize = 256;

impl InboundSocket {
    pub async fn connect(endpoint: impl ToSocketAddrs) -> Result<InboundSocket> {
        let (driver_sender, driver_rx) = mpsc::channel(MAX_QUEUE_SIZE);

        let (tcp_receive, tcp_write) = {
            let tcp = TcpStream::connect(endpoint).await?;
            // Send packages out as soon as possible.
            tcp.set_nodelay(true)?;
            tcp.into_split()
        };

        let mut reader = EventReader::new(tcp_receive);

        // Wait for the initial auth/request event.
        {
            let initial_event = reader
                .read_event()
                .await
                .context("Reading the initial event")?;
            AuthRequest::from_message(initial_event).context("Expecting auth/request")?;
        }

        // Spawn the event reader.
        let reader = {
            let driver_sender = driver_sender.clone();
            spawn(async move {
                let r = dispatch_events(&mut reader, driver_sender).await;
                if let Err(ref e) = r {
                    error!("Reader loop crashed: {}", e);
                }
                debug!("Ending reader loop");
                r
            })
        };

        // Spawn the driver.
        let driver = spawn(async move {
            let mut driver = Driver::new(tcp_write);
            let mut driver_rx = driver_rx;
            loop {
                let message = driver_rx.recv().await;
                if let Some(message) = message {
                    use ProcessingResult::*;
                    match driver.process_message(message).await? {
                        Continue => {
                            continue;
                        }
                        Disconnected(notice) => {
                            debug!("Received disconnect notice: {}", notice);
                            return Ok(Some(notice));
                        }
                    }
                } else {
                    debug!("Receiver channel got closed (all senders dropped)");
                    return Ok(None);
                }
            }
        });

        let socket = InboundSocket {
            reader,
            driver,
            driver_sender,
        };

        Ok(socket)
    }
}

async fn dispatch_events(
    reader: &mut EventReader<OwnedReadHalf>,
    driver: Sender<DriverMessage>,
) -> Result<()> {
    loop {
        let event = reader.read_event().await?;
        driver.send(DriverMessage::Event(event)).await?;
    }
}

/// "Layer 3" inbound socket commands. Concrete commands, full error handling and result conversion.
impl InboundSocket {
    /// Authenticate with a password.
    ///
    /// Returns the info string attached to the reply, if any.
    pub async fn auth(&self, password: impl AsRef<str>) -> Result<Option<String>> {
        self.command(format!("auth {}", password.as_ref())).await
    }

    /// All channels in form of a [`QueryTable`].
    pub async fn channels(&self) -> Result<query::Table> {
        let result = self.api("show channels as json").await?;
        Ok(query::Table::from_json(result.content.as_ref())?)
    }

    /// The number of channels.
    pub async fn channels_count(&self) -> Result<usize> {
        let result = self.api("show channels count as json").await?;
        Ok(query::Count::from_json(result.content.as_ref())?.into())
    }
}

/// "Layer 2" inbound socket send functions. Full error handling.
impl InboundSocket {
    /// Sends a blocking command.
    ///
    /// These all commands except `api` commands. These are expected to return a `Reply-Text`
    /// header.
    ///
    /// - If the reply text starts with `+OK`, returns the info text attached to the `Reply-Text`
    ///   header if any.
    /// - If the reply text starts with `-ERR`, returns an error.
    pub async fn command(&self, cmd: impl AsRef<str>) -> Result<Option<String>> {
        // TODO: consider to predefine all possible commands in an enum?
        let cmd = cmd.as_ref();
        let response = self.send(cmd).await?;
        match CommandReply::from_message(response)?.reply_text {
            ReplyText::Ok(info) => Ok(info),
            ReplyText::Err(info) => bail!("Command `{}` failed: {:?}", cmd, info),
        }
    }

    /// Send a blocking `api` command.
    ///
    /// Returns an error if the [`Message`] returned does not contain a `api/response` content
    /// block.
    pub async fn api(&self, cmd: impl AsRef<str>) -> Result<ApiResponse> {
        let cmd = cmd.as_ref();
        // TODO: Don't support cmd with LFs
        // TODO: encode cmd?
        let message = self.send(format!("api {}", cmd)).await?;
        Ok(ApiResponse::from_message(message)?)
    }
}

/// "Layer 1" inbound socket send function, no support for protocol error handling.
impl InboundSocket {
    /// Send a blocking command to FreeSWITCH.
    ///
    /// Don't use `bgapi` here. If so, this function may not return or screws up the internal state.
    ///
    /// This function does not implement protocol error handling. It returns the answer [`Message`]
    /// as is without looking into it.
    pub async fn send(&self, cmd: impl AsRef<str>) -> Result<Message> {
        let (tx, rx) = oneshot::channel();
        let command = Command::Blocking {
            cmd: cmd.as_ref().to_owned(),
            message: Message::default(),
            responder: tx,
        };
        self.send_command(command).await?;
        // TODO: Support timeouts here?
        Ok(rx.await?)
    }

    /// Sends a command.
    async fn send_command(&self, command: Command) -> Result<()> {
        Ok(self
            .driver_sender
            .send(DriverMessage::Command(command))
            .await?)
    }
}

#[derive(Debug)]
enum Command {
    Blocking {
        cmd: String,
        message: Message,
        responder: oneshot::Sender<Message>,
    },
    Background {
        cmd: String,
        responder: oneshot::Sender<Message>,
    },
    Application {
        cmd: String,
        responder: mpsc::Sender<Message>,
    },
}

#[derive(Debug)]
enum DriverMessage {
    Event(Message),
    Command(Command),
}

#[derive(Debug)]
struct Driver {
    writer: OwnedWriteHalf,
    state: DriverState,
}

#[derive(Default, Debug)]
struct DriverState {
    blocking: VecDeque<oneshot::Sender<Message>>,
    background: HashMap<Uuid, oneshot::Sender<Message>>,
    application: HashMap<Uuid, mpsc::Sender<Message>>,
}

#[derive(Clone, Debug)]
enum ProcessingResult {
    /// Everything is well, processing can continue.
    Continue,
    /// Got a disconnect notice from the service.
    Disconnected(DisconnectNotice),
}

impl Driver {
    pub fn new(writer: OwnedWriteHalf) -> Self {
        let state = DriverState::default();
        Self { writer, state }
    }

    async fn process_message(&mut self, message: DriverMessage) -> Result<ProcessingResult> {
        use self::Command::*;
        use DriverMessage::*;
        match message {
            Event(e) => self.process_event(e),
            Command(Blocking {
                cmd,
                message,
                responder,
            }) => {
                self.state.blocking.push_back(responder);
                self.send_blocking(&cmd, &message).await?;
                Ok(ProcessingResult::Continue)
            }
            Command(Background { cmd, responder }) => {
                let uuid = Uuid::new_v4();
                self.state.background.insert(uuid, responder);
                self.send_background(&cmd, uuid).await?;
                Ok(ProcessingResult::Continue)
            }
            Command(Application {
                cmd: _,
                responder: _,
            }) => {
                todo!("Unsupported");
            }
        }
    }

    async fn send_blocking(&mut self, cmd: &str, message: &Message) -> Result<()> {
        // We first write to an internal buffer, and then write to the socket and flush it
        // immediately after to get the data out as soon as possible (see `TcpStream::set_nodelay`).
        let buf = {
            // TODO: recycle the write buffer?
            // TODO: pre-compute the capacity?
            // TODO: write directly to TcpStream (use its send buffer)?
            let mut buf = Vec::new();
            buf.extend_from_slice(cmd.as_bytes());
            buf.push(LF);
            message.write(&mut buf)?;
            buf
        };

        {
            self.writer.write_all(&buf).await?;
            self.writer.flush().await?;
        }

        Ok(())
    }

    async fn send_background(&mut self, cmd: &str, uuid: Uuid) -> Result<()> {
        Ok(())
    }

    async fn send(&mut self, cmd: &str, attachment: Message) -> Result<()> {
        Ok(())
    }

    fn process_event(&mut self, message: Message) -> Result<ProcessingResult> {
        trace!("Received: {:?}", message);
        // For now we don't support messages without a content type (are there any?).
        if let Some(content_type) = message.content_type()? {
            let content_types = content_types();
            if content_type == content_types.command_reply
                || content_type == content_types.api_response
            {
                self.process_reply(message)?;
                return Ok(ProcessingResult::Continue);
            }

            if content_type == content_types.disconnect_notice {
                let notice = DisconnectNotice::from_message(message)?;
                return Ok(ProcessingResult::Disconnected(notice));
            }

            todo!("Unexpected content type: {}", content_type)
        }
        bail!("Received message without a content type.");
    }

    fn process_reply(&mut self, message: Message) -> Result<()> {
        if let Some(next) = self.state.blocking.pop_front() {
            return next
                .send(message)
                .map_err(|_m| anyhow!("Failed to reply message"));
        }
        bail!("Received a reply message, but without a command / api request that was sent before:\n{:?}", message)
    }
}

struct EventReader<R: AsyncRead + Unpin> {
    read: R,
    socket_buffer: [u8; BUFFER_SIZE],
    // The number of bytes already consumed from the read_buffer.
    read_consumed: usize,
    read_buffer: Vec<u8>,
}

impl<R: AsyncRead + Unpin> EventReader<R> {
    fn new(read: R) -> Self {
        Self {
            read,
            socket_buffer: [0; BUFFER_SIZE],
            read_consumed: 0,
            read_buffer: Default::default(),
        }
    }

    async fn read_event(&mut self) -> Result<Message> {
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
        Ok(Headers::parse(block)?)
    }

    /// Reads until the given bytes have been received. `len` may be 0 which immediately returns an
    /// empty slice.
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

    /// Remove the already consumed data from the read buffer.
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
        let size = self.read.read(&mut self.socket_buffer[..]).await?;
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
