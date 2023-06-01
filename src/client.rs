use crate::{
    event::{ApiResponse, CommandReply, ReplyText},
    event_socket::driver::{Command, DriverMessage},
    hangup_cause::HangupCause,
    query, FromMessage, Message,
};
use anyhow::{bail, Result};
use std::str;
use tokio::sync::{
    mpsc::{self, Sender},
    oneshot,
};

#[derive(Debug, Clone)]
pub struct Client {
    driver_tx: mpsc::Sender<DriverMessage>,
}

impl Client {
    pub(crate) fn new(sender: mpsc::Sender<DriverMessage>) -> Self {
        Self { driver_tx: sender }
    }
}

/// "Layer 3" inbound socket commands. Concrete commands, full error handling and result conversion.
impl Client {
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
impl Client {
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
impl Client {
    /// Send a blocking command to FreeSWITCH.
    ///
    /// Don't use `bgapi` here. If so, this function may not return or screws up the internal state.
    ///
    /// This function does not implement protocol error handling. It returns the answer [`Message`]
    /// as is without looking into it.
    pub async fn send(&self, cmd: impl AsRef<str>) -> Result<Message> {
        send(&self.driver_tx, cmd).await
    }
}

pub async fn send_api(
    driver_tx: &Sender<DriverMessage>,
    cmd: impl AsRef<str>,
) -> Result<ApiResponse> {
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
