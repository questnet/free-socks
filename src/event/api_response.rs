use super::content_types;
use crate::FromMessage;
use anyhow::{bail, Result};

#[derive(Clone, Debug)]
pub struct ApiResponse {
    pub content: Vec<u8>,
}

impl FromMessage for ApiResponse {
    fn from_message(message: crate::Message) -> Result<Self> {
        message.expect_content_type(&content_types().api_response)?;
        if let Some(content) = message.content {
            // TODO: Can we actually assume this is always UTF-8? And should we fail if not.
            Ok(Self {
                content: content.into_data(),
            })
        } else {
            bail!("Seen api/response Content-Type, but no content")
        }
    }
}
