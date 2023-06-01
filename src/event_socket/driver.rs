use crate::{
    event::{content_types, DisconnectNotice},
    FromMessage, Message, LF,
};
use anyhow::{anyhow, bail, Result};
use log::trace;
use std::collections::{HashMap, VecDeque};
use tokio::{
    io::AsyncWriteExt,
    net::tcp::OwnedWriteHalf,
    sync::{mpsc, oneshot},
};
use uuid::Uuid;

#[derive(Debug)]
pub enum DriverMessage {
    Event(Message),
    Command(Command),
}

#[derive(Debug)]
pub enum Command {
    Blocking {
        cmd: String,
        message: Message,
        responder: oneshot::Sender<Message>,
    },
    #[allow(unused)]
    Background {
        cmd: String,
        responder: oneshot::Sender<Message>,
    },
    #[allow(unused)]
    Application {
        cmd: String,
        responder: mpsc::Sender<Message>,
    },
}

#[derive(Debug)]
pub struct Driver {
    writer: OwnedWriteHalf,
    state: DriverState,
}

#[derive(Default, Debug)]
struct DriverState {
    blocking: VecDeque<oneshot::Sender<Message>>,
    background: HashMap<Uuid, oneshot::Sender<Message>>,
    #[allow(unused)]
    application: HashMap<Uuid, mpsc::Sender<Message>>,
}

#[derive(Clone, Debug)]
pub enum ProcessingResult {
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

    pub async fn process_message(&mut self, message: DriverMessage) -> Result<ProcessingResult> {
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

    async fn send_background(&mut self, _cmd: &str, _uuid: Uuid) -> Result<()> {
        todo!()
    }

    #[allow(unused)]
    async fn send(&mut self, _cmd: &str, _attachment: Message) -> Result<()> {
        todo!()
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
