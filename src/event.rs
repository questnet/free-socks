use crate::Event;
use anyhow::Result;

mod content;
pub mod event;
mod filter_manager;
pub mod ty;

pub use content::*;

/// This trait in implemented for types that can be extracted from an [`Event`].
pub trait FromEvent: Sized {
    fn from_event(event: &Event) -> Result<Self>;
}
