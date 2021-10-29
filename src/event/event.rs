use crate::{sequence, ThenSome, LF};
use anyhow::{bail, Result};
use mime::Mime;
use std::str;

#[derive(Clone, Default, Debug)]
pub struct Event {
    pub headers: Headers,
    pub content: Option<Content>,
}

#[derive(Clone, Default, Debug)]
pub struct Headers(pub Vec<Header>);

impl Headers {
    pub fn parse(block: &[u8]) -> Result<Self> {
        let mut headers = Vec::new();
        const NAME_VALUE_SEPARATOR: &[u8; 2] = b": ";
        for line in block.split(|b| *b == LF) {
            if let Some(index) = sequence::find_first(line, NAME_VALUE_SEPARATOR) {
                if index == 0 {
                    bail!("Empty header name: {}", String::from_utf8_lossy(line))
                }
                let name = str::from_utf8(&line[..index])?.to_owned();
                let value = str::from_utf8(&line[index + NAME_VALUE_SEPARATOR.len()..])?.to_owned();
                headers.push(Header { name, value })
            } else {
                bail!(
                    "Failed to find ': ' separator in header line: {}",
                    String::from_utf8_lossy(line)
                )
            }
        }
        Ok(Self(headers))
    }

    pub fn value(&self, name: impl AsRef<str>) -> Option<&str> {
        let name = name.as_ref();
        self.0
            .iter()
            .find_map(|h| (h.name == name).then_some(h.value.as_str()))
    }

    pub fn content(&self) -> Result<Option<(Mime, usize)>> {
        let ty = self.value("Content-Type");
        let len = self.value("Content-Length");
        match (ty, len) {
            (None, None) => Ok(None),
            (Some(_), None) => bail!("Seen Content-Type but no Content-Length"),
            (None, Some(_)) => bail!("Seen Content-Length but no Content-Type"),
            (Some(ty), Some(len)) => {
                let ty = ty.parse::<Mime>()?;
                let len = len.parse::<usize>()?;
                Ok(Some((ty, len)))
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Header {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug)]
pub struct Content {
    pub ty: Mime,
    pub data: Vec<u8>,
}
