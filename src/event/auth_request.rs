use crate::event::content_types;
use crate::FromMessage;

#[derive(Clone, Debug)]
pub struct AuthRequest;

impl FromMessage for AuthRequest {
    fn from_message(message: crate::Message) -> anyhow::Result<Self> {
        message.expect_content_type(&content_types().auth_request)?;
        Ok(Self)
    }
}
