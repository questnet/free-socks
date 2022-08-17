//! # FreeSWITCH inbound socket.
//!
//! ADR:
//! Initially we thought about creating a design that polls the events and the commands at the same
//! time, but it turned out that select! not simple to use on futures that are not cancel safe
//! (which our event reader isn't, because of the intermediate buffers).
//!
//! To remedy that, a channel mpsc queue based approach was used. The event reader and the command
//! sender push both to the same queue which gets processed by a driver.
use crate::{
    event::{ApiResponse, AuthRequest, CommandReply, DisconnectNotice, ReplyText},
    hangup_cause::HangupCause,
    query, FromMessage, Message,
};
use anyhow::{bail, Context, Result};
use driver::{Command, Driver, ProcessingResult};
use event_reader::EventReader;
use log::{debug, error, info};
use std::{fmt, str, sync::Mutex};
use strum::Display;
use tokio::{
    net::{tcp::OwnedReadHalf, TcpStream, ToSocketAddrs},
    select,
    sync::{
        mpsc::{self, Receiver, Sender},
        oneshot,
    },
};

mod driver;
mod event_reader;

/// A connected FreeSWITCH event socket.
pub struct EventSocket {
    state: Mutex<State>,
}

const MAX_QUEUE_SIZE: usize = 256;

#[derive(Clone, PartialEq, Eq, Debug, Display)]
pub enum ConnectionState {
    Connected,
    Disconnected(Option<DisconnectNotice>),
}

impl EventSocket {
    pub async fn connect(endpoint: impl ToSocketAddrs) -> Result<EventSocket> {
        let (message_sender, message_receiver) = mpsc::channel(MAX_QUEUE_SIZE);
        let (command_sender, command_receiver) = mpsc::channel(MAX_QUEUE_SIZE);

        let (tcp_receive, tcp_write) = {
            let tcp = TcpStream::connect(endpoint).await?;
            // Send network packets out as soon as possible.
            tcp.set_nodelay(true)?;
            tcp.into_split()
        };

        let mut reader = EventReader::new(tcp_receive);

        // Wait for the initial `auth/request`.
        {
            let initial_request = reader
                .read_event()
                .await
                .context("Reading the initial request")?;
            AuthRequest::from_message(initial_request).context("Expecting auth/request")?;
        }

        let reader = {
            async move {
                let r = read_and_dispatch_messages(reader, message_sender).await;
                if let Err(ref e) = r {
                    error!("Reader loop crashed: {e}");
                }
                debug!("Ending reader loop");
                r
            }
        };

        // Running the driver loop.
        // The driver is either getting messages from the event receiver. Or external commands.
        let driver = async move {
            let driver = Driver::new(tcp_write);
            match dispatch_commands_and_messages(command_receiver, message_receiver, driver).await {
                Ok(()) => {}
                Err(_) => {}
            }
        };

        // We don't need to use the JoinHandles. The tasks will terminate as soon the
        // command sender / EventSocket gets dropped.
        tokio::spawn(reader);
        tokio::spawn(driver);

        Ok(EventSocket { command_sender })
    }

    /// Returns the current connection state of the event socket. This is initially connected but
    /// may change at any time to disconnected.
    ///
    /// This is useful to find out why a command failed and if it makes sense to reconnect.
    pub fn connection_state(&self) -> ConnectionState {
        use State::*;
        match *self.state.lock().expect("Poisoned mutex") {
            Connected(_) => ConnectionState::Connected,
            Disconnected(notice) => ConnectionState::Disconnected(notice),
        }
    }
}

async fn read_and_dispatch_messages(
    mut reader: EventReader<OwnedReadHalf>,
    message_sender: Sender<Message>,
) -> Result<(), anyhow::Error> {
    loop {
        let event = reader.read_event().await?;
        message_sender.send(event).await?;
    }
}

async fn dispatch_commands_and_messages(
    mut command_receiver: Receiver<Command>,
    mut message_receiver: Receiver<Message>,
    mut driver: Driver,
) -> Result<()> {
    loop {
        select! {
            message = message_receiver.recv() => {
                if let Some(message) = message {
                    use ProcessingResult::*;
                    match driver.process_message(message)? {
                        Continue => {},
                        Disconnected(notice) =>
                        {
                            debug!("Received disconnect notice: {notice}");
                            return Ok(());
                        }
                    }
                } else {
                    info!("Message senders were dropped, ending dispatcher.");
                    return Ok(())
                }
            }
            command = command_receiver.recv() => {
                if let Some(command) = command {
                    driver.execute_command(command).await?;
                } else {
                    info!("Command senders were dropped, ending dispatcher.");
                    return Ok(())
                }
            }
        }
    }
}

#[derive(Debug)]
enum State {
    Connected(Sender<Command>),
    Disconnected(Option<DisconnectNotice>),
}

/// "Layer 3" inbound socket commands. Concrete commands, full error handling and result conversion.
impl EventSocket {
    /// Authenticate with a password.
    ///
    /// Returns the info string attached to the reply, if any.
    pub async fn auth(&self, password: impl AsRef<str>) -> Result<Option<String>> {
        self.command(format!("auth {}", password.as_ref())).await
    }

    /// Get all channels in form of a [`query::Table`].
    pub async fn channels(&self, like: impl Into<Option<&str>>) -> Result<query::Table> {
        let like = like.into();
        let result = match like {
            Some(like) => {
                let like = validate_and_escape_like_literal(like)?;
                self.api(format!("show channels like {like} as json"))
                    .await?
            }
            None => self.api("show channels as json").await?,
        };
        query::Table::from_json(result.content.as_ref())
    }

    pub async fn hupall(
        &self,
        cause: HangupCause,
        variable_matches: &[(&str, &str)],
    ) -> Result<()> {
        if variable_matches.len() > 5 {
            // https://github.com/signalwire/freeswitch/blob/v1.10.7/src/mod/applications/mod_commands/mod_commands.c#L6688
            bail!("`hupall` can match a maximum of 5 variables");
        }

        let var_args = {
            let mut args = Vec::new();
            for (name, value) in variable_matches {
                args.push(escape_argument(name)?);
                args.push(escape_argument(value)?);
            }
            args.join(" ")
        };

        self.api(format!("hupall {} {}", cause, var_args)).await?;

        Ok(())
    }

    /// The number of channels.
    pub async fn channels_count(&self) -> Result<usize> {
        let result = self.api("show channels count as json").await?;
        Ok(query::Count::from_json(result.content.as_ref())?.into())
    }
}

fn validate_and_escape_like_literal(str: &str) -> Result<String> {
    if str.contains('\'') || str.contains(';') {
        bail!("'like' strings can not contain `'` or `;`");
    }

    escape_argument(str)
}

/// Escape all characters.
///
/// see `cleanup_separated_string()` and `unescape_char()` in `switch_util.c`.
fn escape_argument(str: impl AsRef<str>) -> Result<String> {
    let str = str.as_ref();
    let mut result = Vec::with_capacity(str.len() + 8);

    // This should be UTF-8 safe. the most significant bit is always set for extension bytes that
    // are part of an encoded unicode character.
    str.as_bytes().iter().copied().for_each(|c| {
        let as_is = &[c];
        let escaped: &[u8] = match c {
            b'\'' => b"\\'",
            b'"' => b"\\\"",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            b' ' => b"\\s",
            _ => as_is,
        };

        result.extend(escaped);
    });

    Ok(String::from_utf8(result)?)
}

/// "Layer 2" inbound socket send functions. Full error handling.
impl EventSocket {
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
            ReplyText::Err(info) => {
                let info = info.map(|info| ": ".to_owned() + &info).unwrap_or_default();
                bail!("Command `{cmd}` failed{}", info)
            }
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
        let message = self.send(format!("api {cmd}")).await?;
        ApiResponse::from_message(message)
    }
}

/// "Layer 1" inbound socket send function, no support for protocol error handling.
impl EventSocket {
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
        
        Ok(self.command_sender.send(command).await?
    }
}

#[cfg(test)]
mod test {
    fn after_closing_the_socket_the_state_is_disconnected() {
        todo!()
    }
}
