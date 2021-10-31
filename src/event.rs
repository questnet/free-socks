use crate::Message;
use anyhow::Result;

mod content;
mod filter_manager;
pub mod message;
pub mod ty;

pub use content::*;

/// This trait in implemented for types that can be extracted from an [`Message`].
pub trait FromMessage: Sized {
    fn from_message(message: &Message) -> Result<Self>;
}
