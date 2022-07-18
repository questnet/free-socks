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
    event::{ApiResponse, AuthRequest, CommandReply, DisconnectNotice, ReplyText},
    hangup_cause::HangupCause,
    query, FromMessage, Message,
};
use anyhow::{bail, Context, Result};
use driver::{Command, Driver, DriverMessage, ProcessingResult};
use event_reader::EventReader;
use log::{debug, error};
use std::str;
use tokio::{
    net::{tcp::OwnedReadHalf, TcpStream, ToSocketAddrs},
    spawn,
    sync::{
        mpsc::{self, Sender},
        oneshot,
    },
    task::JoinHandle,
};

mod driver;
mod event_reader;

#[derive(Debug)]
pub struct EventSocket {
    driver_sender: mpsc::Sender<DriverMessage>,
    reader: JoinHandle<Result<()>>,
    driver: JoinHandle<Result<Option<DisconnectNotice>>>,
}

impl Drop for EventSocket {
    fn drop(&mut self) {
        self.reader.abort();
        self.driver.abort();
    }
}

const MAX_QUEUE_SIZE: usize = 256;

impl EventSocket {
    pub async fn connect(endpoint: impl ToSocketAddrs) -> Result<EventSocket> {
        let (driver_sender, driver_rx) = mpsc::channel(MAX_QUEUE_SIZE);

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

        let socket = EventSocket {
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
                self.api(format!("show channels like {} as json", like))
                    .await?
            }
            None => self.api("show channels as json").await?,
        };
        Ok(query::Table::from_json(result.content.as_ref())?)
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
                args.push(escape_argument(name.to_string())?);
                args.push(escape_argument(value.to_string())?);
            }
            args.join(" ")
        };

        self.api(format!("hupall {} {}", cause.to_string(), var_args))
            .await?;

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
                bail!("Command `{}` failed{}", cmd, info)
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
        let message = self.send(format!("api {}", cmd)).await?;
        Ok(ApiResponse::from_message(message)?)
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
        Ok(self
            .driver_sender
            .send(DriverMessage::Command(command))
            .await?)
    }
}
