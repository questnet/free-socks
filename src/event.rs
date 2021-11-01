use crate::Message;
use anyhow::{bail, Result};
use derive_more::Display;
use mime::Mime;
use once_cell::sync::OnceCell;
use std::str::FromStr;

mod api_response;
mod auth_request;
mod command_reply;
mod filter_manager;
pub mod message;
pub mod ty;

pub use api_response::*;
pub use auth_request::*;
pub use command_reply::*;

/// This trait in implemented for values that can be extracted from an [`Message`].
pub trait FromMessage: Sized {
    fn from_message(message: Message) -> Result<Self>;
}

/// Well known FreeSWITCH content types.
#[derive(Clone, Debug)]
pub struct ContentTypes {
    pub command_reply: Mime,
    pub api_response: Mime,
    pub auth_request: Mime,
    pub disconnect_notice: Mime,
}

pub fn content_types() -> &'static ContentTypes {
    static CONTENT_TYPES: OnceCell<ContentTypes> = OnceCell::new();
    CONTENT_TYPES.get_or_init(|| ContentTypes {
        command_reply: Mime::from_str("command/reply").unwrap(),
        api_response: Mime::from_str("api/response").unwrap(),
        auth_request: Mime::from_str("auth/request").unwrap(),
        disconnect_notice: Mime::from_str("text/disconnect-notice").unwrap(),
    })
}

#[derive(Clone, Debug, Display)]
pub struct DisconnectNotice(String);

impl FromMessage for DisconnectNotice {
    fn from_message(message: Message) -> Result<Self> {
        message.expect_content_type(&content_types().disconnect_notice)?;
        if let Some(content) = message.content {
            Ok(Self(content.into_string()?))
        } else {
            bail!("Expect disconnect notice, but there is no content block in this message.")
        }
    }
}
