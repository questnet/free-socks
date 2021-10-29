use crate::{FromEvent, ThenSome};
use anyhow::Result;
use mime::Mime;
use regex::Regex;
use std::str::FromStr;
use thiserror::Error;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CommandReply {
    pub reply_text: ReplyText,
}

impl FromEvent for CommandReply {
    fn from_event(event: &crate::Event) -> anyhow::Result<Self> {
        // TODO: Pre-create well known Mime types.
        event.expect_content_type(Mime::from_str("command/reply")?)?;
        let reply_text: ReplyText = event.get("Reply-Text")?;
        Ok(CommandReply { reply_text })
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ReplyText {
    Ok(Option<String>),
    Err(Option<String>),
}

/// A simple parse error.
///
/// That's because we can't return [`anyhow::Result`] from [`FromStr::from_str`].
/// See
///
/// - https://github.com/dtolnay/anyhow/pull/163
/// - https://github.com/dtolnay/anyhow/issues/172
#[derive(Error, Debug)]
pub enum ParseError {
    #[error("Parse error: `{0}`")]
    ParseError(String),
}

impl FromStr for ReplyText {
    type Err = ParseError;

    fn from_str(text: &str) -> Result<Self, ParseError> {
        // TODO: Is this pre-compiled?
        let re = Regex::new(r"^(\+OK|\-ERR)(.*)$").unwrap();
        let captures = re
            .captures(text)
            .ok_or_else(|| ParseError::ParseError(text.to_string()))?;
        if captures.len() != 3 {
            return Err(ParseError::ParseError(text.to_string()));
        }
        // Match must be exhaustive.
        assert_eq!(captures[0].len(), text.len());

        let info = {
            let info = &captures[2];
            if !info.is_empty() && !(info.starts_with(' ')) {
                // Info, if any, must begin with a space.
                return Err(ParseError::ParseError(text.to_string()));
            }
            let info = info.trim();
            (!info.is_empty()).then_some(info.to_string())
        };
        Ok(match &captures[1] {
            "+OK" => ReplyText::Ok(info),
            "-ERR" => ReplyText::Err(info),
            _ => panic!("Internal error"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_text_ok() {
        assert_eq!("+OK".parse::<ReplyText>().unwrap(), ReplyText::Ok(None));
        assert_eq!("+OK ".parse::<ReplyText>().unwrap(), ReplyText::Ok(None));
        assert_eq!("+OK  ".parse::<ReplyText>().unwrap(), ReplyText::Ok(None));
    }

    #[test]
    fn reply_text_err() {
        assert_eq!("-ERR".parse::<ReplyText>().unwrap(), ReplyText::Err(None));
        assert_eq!("-ERR ".parse::<ReplyText>().unwrap(), ReplyText::Err(None));
        assert_eq!("-ERR  ".parse::<ReplyText>().unwrap(), ReplyText::Err(None));
    }

    #[test]
    fn reply_text_with_info() {
        assert_eq!(
            "+OK x".parse::<ReplyText>().unwrap(),
            ReplyText::Ok(Some("x".to_string()))
        );
        assert_eq!(
            "+OK  x ".parse::<ReplyText>().unwrap(),
            ReplyText::Ok(Some("x".to_string()))
        );
    }

    #[test]
    fn failures() {
        assert!("+OKx".parse::<ReplyText>().is_err());
        assert!("-ERRx".parse::<ReplyText>().is_err());
        assert!("+ERR".parse::<ReplyText>().is_err());
    }
}
