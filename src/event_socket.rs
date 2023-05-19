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
    event_socket::driver::ProcessingResult,
    hangup_cause::HangupCause,
    query, FromMessage, Message,
};
use anyhow::{bail, Context, Result};
use futures::{Future, FutureExt};
use log::{debug, error, trace};
use std::{future::pending, str, time::Duration};
use tokio::{
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpStream, ToSocketAddrs,
    },
    select, spawn,
    sync::{
        mpsc::{self, Receiver, Sender},
        oneshot,
    },
    task::{JoinError, JoinHandle},
    time,
};

use self::{
    driver::{Command, Driver, DriverMessage},
    event_reader::EventReader,
};

mod driver;
mod event_reader;

#[derive(Debug)]
pub struct EventSocket {
    driver_tx: mpsc::Sender<DriverMessage>,
    event_socket: JoinHandle<Result<Option<DisconnectNotice>>>,
}

impl Drop for EventSocket {
    fn drop(&mut self) {
        self.event_socket.abort()
    }
}

/// `EventSocket` is itself a future that completes when the connection to the FreeSWITCH event
/// socket is closed. It does not need to be polled for the event socket to execute.
impl Future for EventSocket {
    type Output = std::result::Result<Result<Option<DisconnectNotice>>, JoinError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.event_socket.poll_unpin(cx)
    }
}

const MAX_QUEUE_SIZE: usize = 256;

impl EventSocket {
    pub async fn connect(
        endpoint: impl ToSocketAddrs,
        keepalive_interval: impl Into<Option<Duration>>,
    ) -> Result<EventSocket> {
        let (driver_sender, driver_rx) = mpsc::channel(MAX_QUEUE_SIZE);

        let (tcp_receive, tcp_write) = {
            let tcp = TcpStream::connect(endpoint).await?;
            // Flush network packets as soon as possible.
            tcp.set_nodelay(true)?;
            tcp.into_split()
        };

        let mut reader = EventReader::new(tcp_receive);

        // Wait for the initial `auth/request`.
        {
            let initial_request = reader
                .read_event()
                .await
                .context("Waiting for the initial auth request")?;
            AuthRequest::from_message(initial_request).context("Expecting auth/request")?;
        }

        // Spawn the event socket.
        //
        // ADR: This is for convenience so that callers don't need to drive the async function and
        // can simply send API requests.
        let driver_tx_socket = driver_sender.clone();
        let keepalive_interval = keepalive_interval.into();
        let event_socket = spawn(async move {
            event_socket(
                keepalive_interval,
                reader,
                tcp_write,
                driver_rx,
                driver_tx_socket,
            )
            .await
        });

        let socket = EventSocket {
            driver_tx: driver_sender,
            event_socket,
        };

        Ok(socket)
    }
}

/// The primary async function that represents the event socket. Its returned future must be polled
/// until it completes indicating that the connection to the FreeSWITCH event socket terminated.
async fn event_socket(
    keepalives: Option<Duration>,
    mut reader: EventReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    driver_rx: Receiver<DriverMessage>,
    driver_tx: Sender<DriverMessage>,
) -> Result<Option<DisconnectNotice>> {
    let reader = dispatch_events(&mut reader, driver_tx.clone());
    let driver = dispatch_messages(driver_rx, writer);

    let send_keepalives = {
        if let Some(keepalive_duration) = keepalives {
            debug!(
                "Enabling keepalives with a duration of {:?}",
                keepalive_duration
            );
            send_keepalives(driver_tx, keepalive_duration).boxed()
        } else {
            debug!("Keepalives disabled");
            pending::<Result<()>>().boxed()
        }
    };

    select! {
        r = reader => {
            debug!("Reader loop ended");
            if let Err(ref e) = r {
                error!("Reader loop crashed: {}", e);
            }
            r.map(|()| None)
        }
        r = driver => {
            debug!("Driver loop ended");
            if let Err(ref e) = r {
                error!("Driver loop crashed: {}", e);
            }
            r
        }
        r = send_keepalives => {
            // Note that the keepalive loop ends only in case of an error.
            debug!("Keepalive loop ended");
            if let Err(ref e) = r {
                error!("Keepalive loop crashed: {}", e);
            }
            r.map(|()| None)
        }
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

async fn dispatch_messages(
    receiver: Receiver<DriverMessage>,
    tcp_write: OwnedWriteHalf,
) -> Result<Option<DisconnectNotice>> {
    let mut driver = Driver::new(tcp_write);
    let mut receiver = receiver;
    loop {
        let message = receiver.recv().await;
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
}

async fn send_keepalives(driver_tx: Sender<DriverMessage>, duration: Duration) -> Result<()> {
    loop {
        time::sleep(duration).await;
        // TODO: Really need timeouts here, what if FreeSWITCH crashes before responding?
        trace!("Sending keepalive");
        send_api(&driver_tx, "echo keepalive").await?;
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
        send_api(&self.driver_tx, cmd).await
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
        send(&self.driver_tx, cmd).await
    }

    /// Sends a command.
    async fn send_command(&self, command: Command) -> Result<()> {
        send_command(&self.driver_tx, command).await
    }
}

async fn send_api(driver_tx: &Sender<DriverMessage>, cmd: impl AsRef<str>) -> Result<ApiResponse> {
    let cmd = cmd.as_ref();
    // TODO: Don't support cmd with LFs
    // TODO: encode cmd?
    let message = send(driver_tx, format!("api {}", cmd)).await?;
    Ok(ApiResponse::from_message(message)?)
}

async fn send(driver_tx: &Sender<DriverMessage>, cmd: impl AsRef<str>) -> Result<Message> {
    let (tx, rx) = oneshot::channel();
    let command = Command::Blocking {
        cmd: cmd.as_ref().to_owned(),
        message: Message::default(),
        responder: tx,
    };
    send_command(driver_tx, command).await?;
    // TODO: Support timeouts here?
    Ok(rx.await?)
}

async fn send_command(driver_tx: &Sender<DriverMessage>, command: Command) -> Result<()> {
    Ok(driver_tx.send(DriverMessage::Command(command)).await?)
}
