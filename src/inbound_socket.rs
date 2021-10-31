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
    event::{CommandReply, ReplyText},
    sequence, Content, FromMessage, Headers, Message, LF,
};
use anyhow::{bail, Result};
use log::debug;
use std::{
    collections::{HashMap, VecDeque},
    mem::{self},
    net::SocketAddr,
    str::{self},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpStream,
    },
    spawn,
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use uuid::Uuid;

const BUFFER_SIZE: usize = 0x4000;

pub struct InboundSocket {
    driver_sender: mpsc::Sender<DriverMessage>,
    reader: JoinHandle<Result<()>>,
    driver: JoinHandle<Result<()>>,
}

impl Drop for InboundSocket {
    fn drop(&mut self) {
        self.reader.abort();
        self.driver.abort();
    }
}

const MAX_QUEUE_SIZE: usize = 256;

impl InboundSocket {
    pub async fn connect(addr: impl Into<SocketAddr>) -> Result<InboundSocket> {
        let (driver_sender, driver_rx) = mpsc::channel(MAX_QUEUE_SIZE);

        let (tcp_receive, tcp_write) = {
            let tcp = TcpStream::connect(addr.into()).await?;
            // Send packages out as soon as possible.
            tcp.set_nodelay(true)?;
            tcp.into_split()
        };

        let mut reader = EventReader {
            read: tcp_receive,
            socket_buffer: [0; BUFFER_SIZE],
            read_consumed: 0,
            read_buffer: Default::default(),
        };

        let reader = {
            let driver_sender = driver_sender.clone();
            spawn(async move {
                loop {
                    let event = reader.read_event().await?;
                    driver_sender.send(DriverMessage::Event(event)).await?;
                }
            })
        };

        let driver = spawn(async move {
            let mut driver = Driver::new(tcp_write);
            let mut driver_rx = driver_rx;
            loop {
                let message = driver_rx.recv().await;
                if let Some(message) = message {
                    driver.process_message(message).await?;
                } else {
                    debug!("Driver channel got closed. Ending driver loop.");
                    break;
                }
            }
            Ok(())
        });

        let socket = InboundSocket {
            reader,
            driver,
            driver_sender,
        };

        Ok(socket)
    }
}

/// "Layer 3" inbound socket commands. Concrete commands, full error handling and result conversion.
impl InboundSocket {
    /// Authenticate.
    pub async fn auth(&self, password: impl AsRef<str>) -> Result<()> {
        let event = self.api(format!("auth {}", password.as_ref())).await?;
        todo!();
    }
}

/// "Layer 2" inbound socket send functions. Full error handling.
impl InboundSocket {
    /// Sends a blocking command.
    ///
    /// These all commands except `api` commands. These are expected to return a `Reply-Text`
    /// header.
    ///
    /// - If the reply text starts with `+OK`, returns the info text attached to the Reply-Text`
    ///   header if any.
    /// - If the reply text starts with `-ERR`, returns an error.
    pub async fn command(&self, cmd: impl AsRef<str>) -> Result<Option<String>> {
        // TODO: consider to predefine all possible commands in an enum?
        let cmd = cmd.as_ref();
        let response = self.send(cmd).await?;
        match CommandReply::from_message(&response)?.reply_text {
            ReplyText::Ok(info) => Ok(info),
            ReplyText::Err(info) => bail!("Command `{}` failed: {:?}", cmd, info),
        }
    }

    /// Send a blocking api command.
    ///
    /// Returns in an error if the returned [`Message`] does not contain a `api/response` content
    /// block.
    pub async fn api(&self, cmd: impl AsRef<str>) -> Result<Message> {
        let cmd = cmd.as_ref();
        // TODO: Don't support cmd with LFs
        // TODO: encode cmd?
        let response = self.send(format!("api {}", cmd)).await?;
        todo!()
    }
}

/// "Layer 1" inbound socket send function, no support for protocol error handling.
impl InboundSocket {
    /// Send a blocking command to FreeSWITCH.
    ///
    /// Don't use `bgapi` here. If so, this function may not return or screws up the internal state.
    ///
    /// This function does not implement protocol error handling. It returns the answer [`Event`] as
    /// it is without looking into it.
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

impl Driver {
    pub fn new(writer: OwnedWriteHalf) -> Self {
        let state = DriverState::default();
        Self { writer, state }
    }

    async fn process_message(&mut self, message: DriverMessage) -> Result<()> {
        use self::Command::*;
        use DriverMessage::*;
        match message {
            Event(e) => self.process_event(e)?,
            Command(Blocking {
                cmd,
                message,
                responder,
            }) => {
                self.state.blocking.push_back(responder);
                self.send_blocking(&cmd, &message).await?;
            }
            Command(Background { cmd, responder }) => {
                let uuid = Uuid::new_v4();
                self.state.background.insert(uuid, responder);
                self.send_background(&cmd, uuid).await?;
            }
            Command(Application { cmd, responder }) => {
                todo!("Unsupported");
            }
        }

        Ok(())
    }

    async fn send_blocking(&mut self, cmd: &str, message: &Message) -> Result<()> {
        // We first write to an internal buffer, and then write to the socket and flush it
        // immediately after to get the data out as soon as possible (see `TcpStream::set_nodelay`).
        // TODO: recycle the write buffer?
        // TODO: pre-compute the capacity?
        // TODO: write directly to TcpStream? (does it have a send buffer)?
        let buf = {
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

    fn process_event(&mut self, event: Message) -> Result<()> {
        Ok(())
    }
}

struct EventReader {
    read: OwnedReadHalf,
    socket_buffer: [u8; BUFFER_SIZE],
    // The number of bytes already consumed from the read_buffer.
    read_consumed: usize,
    read_buffer: Vec<u8>,
}

impl EventReader {
    async fn read_event(&mut self) -> Result<Message> {
        let headers = self.read_headers().await?;
        let content = {
            if let Some((ty, len)) = headers.content()? {
                let data = self.read_data(len).await?.to_vec();
                Some(Content { ty, data })
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
            if self.read_buffer.len() <= len {
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
                start_search = self.read_buffer.len() - sequence.len() + 1;
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
