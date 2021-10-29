//# Low lower inbound socket.
// ADR:
// Initially we thought about creating a design that polls the events and the commands at the same
// time, but it turned out that select! not simple to use on futures that are not cancellation safe
// (which our event reader isn't, because of the intermediate buffers).
//
// To remedy that, a channel mpsc queue based approach was used. The event reader and the command
// sender push both to the same queue which gets processed by a driver.

use anyhow::{bail, Result};
use log::debug;
use mime::Mime;
use std::{
    collections::{HashMap, VecDeque},
    mem::{self},
    net::SocketAddr,
    str,
};
use tokio::{
    io::AsyncReadExt,
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

const LF: u8 = b'\n';
const MAX_QUEUE_SIZE: usize = 256;

impl InboundSocket {
    pub async fn connect(addr: impl Into<SocketAddr>) -> Result<InboundSocket> {
        let (driver_sender, driver_rx) = mpsc::channel(MAX_QUEUE_SIZE);

        let (tcp_receive, tcp_write) = {
            let tcp = TcpStream::connect(addr.into()).await?;
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

/// "Layer 2" inbound socket send functions.
impl InboundSocket {
    /// Send a blocking command prefixed with `api`.
    pub async fn api(&self, cmd: impl AsRef<str>) -> Result<Event> {
        todo!()
    }
}

/// "Layer 1" inbound socket send function, no support for protocol error handling.
impl InboundSocket {
    /// Send a blocking command to FreeSWITCH.
    ///
    /// Don't use `bgapi` here. If so, this function may not return or screws up the internal state.
    ///
    /// This function does not implement protocol error handling. It returns the answer as it is
    /// without looking into it.
    pub async fn send(&self, cmd: impl AsRef<str>) -> Result<Event> {
        let (tx, rx) = oneshot::channel();
        let command = Command::Blocking {
            cmd: cmd.as_ref().to_owned(),
            responder: tx,
        };
        self.send_command(command).await?;
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
        responder: oneshot::Sender<Event>,
    },
    Background {
        cmd: String,
        responder: oneshot::Sender<Event>,
    },
    Application {
        cmd: String,
        responder: mpsc::Sender<Event>,
    },
}

#[derive(Debug)]
enum DriverMessage {
    Event(Event),
    Command(Command),
}

#[derive(Debug)]
struct Driver {
    writer: OwnedWriteHalf,
    state: DriverState,
}

#[derive(Default, Debug)]
struct DriverState {
    blocking: VecDeque<oneshot::Sender<Event>>,
    background: HashMap<Uuid, oneshot::Sender<Event>>,
    application: HashMap<Uuid, mpsc::Sender<Event>>,
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
            Command(Blocking { cmd, responder }) => {
                self.state.blocking.push_back(responder);
                self.send_blocking(&cmd).await?;
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

    async fn send_blocking(&mut self, cmd: &str) -> Result<()> {
        Ok(())
    }

    async fn send_background(&mut self, cmd: &str, uuid: Uuid) -> Result<()> {
        Ok(())
    }

    async fn send(&mut self, cmd: &str, attachment: Event) -> Result<()> {
        Ok(())
    }

    fn process_event(&mut self, event: Event) -> Result<()> {
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
    async fn run(&mut self) -> Result<()> {
        loop {
            let event = self.read_event().await?;
        }
    }

    async fn read_event(&mut self) -> Result<Event> {
        let headers = self.read_headers().await?;
        let content = {
            if let Some((ty, len)) = headers.content()? {
                let data = self.read_data(len).await?.to_vec();
                Some(Content { ty, data })
            } else {
                None
            }
        };

        Ok(Event { headers, content })
    }

    async fn read_headers(&mut self) -> Result<Headers> {
        let block = self.read_block().await?;
        Ok(Headers::new(&parse_headers(block)?))
    }

    /// Reads until the given bytes have been received. `len` may be 0 which immediately returns an
    /// empty slice.
    async fn read_data(&mut self, len: usize) -> Result<&[u8]> {
        self.flush_read_buffer();

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
        self.flush_read_buffer();
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

    fn flush_read_buffer(&mut self) {
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

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct Header {
    name: String,
    value: String,
}

fn parse_headers(block: &[u8]) -> Result<Vec<Header>> {
    let mut headers = Vec::new();
    const NAME_VALUE_SEPARATOR: &[u8; 2] = b": ";
    for line in block.split(|b| *b == LF) {
        if let Some(index) = sequence::find_first(line, NAME_VALUE_SEPARATOR) {
            if index == 0 {
                bail!("Empty header name: {}", String::from_utf8_lossy(line))
            }
            let name = str::from_utf8(&line[..index])?.to_owned();
            let value = str::from_utf8(&line[index + NAME_VALUE_SEPARATOR.len()..])?.to_owned();
            headers.push(Header { name, value })
        } else {
            bail!(
                "Failed to find ': ' separator in header line: {}",
                String::from_utf8_lossy(line)
            )
        }
    }
    Ok(headers)
}

#[derive(Clone, Default, Debug)]
pub struct Event {
    headers: Headers,
    content: Option<Content>,
}

#[derive(Clone, Default, Debug)]
struct Headers(Vec<Header>);

impl Headers {
    pub fn new(headers: &[Header]) -> Headers {
        Headers(headers.to_vec())
    }

    pub fn value(&self, name: impl AsRef<str>) -> Option<&str> {
        let name = name.as_ref();
        self.0.iter().find_map(|h| {
            if h.name == name {
                Some(h.value.as_str())
            } else {
                None
            }
        })
    }

    fn content(&self) -> Result<Option<(Mime, usize)>> {
        let ty = self.value("Content-Type");
        let len = self.value("Content-Length");
        match (ty, len) {
            (None, None) => Ok(None),
            (Some(_), None) => bail!("Seen Content-Type but no Content-Length"),
            (None, Some(_)) => bail!("Seen Content-Length but no Content-Type"),
            (Some(ty), Some(len)) => {
                let ty = ty.parse::<Mime>()?;
                let len = len.parse::<usize>()?;
                Ok(Some((ty, len)))
            }
        }
    }
}

#[derive(Clone, Debug)]
struct Content {
    ty: Mime,
    data: Vec<u8>,
}

mod sequence {
    pub fn find_first(all: &[u8], sequence: &[u8]) -> Option<usize> {
        all.windows(sequence.len()).position(|w| *w == *sequence)
    }
}
