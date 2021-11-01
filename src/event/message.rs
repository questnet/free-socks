use crate::{sequence, ThenSome, LF};
use anyhow::{anyhow, bail, Result};
use mime::Mime;
use std::{
    error::Error,
    io::Write,
    str::{self, FromStr},
};

#[derive(Clone, Default, Debug)]
pub struct Message {
    pub headers: Headers,
    pub content: Option<Content>,
}

/// Convenience functions.
impl Message {
    /// Expects the given content type.
    ///
    /// Returns an error if the `Content-Type` header is not set or another content type was found.
    pub fn expect_content_type(&self, expected: &Mime) -> Result<()> {
        let ct = self.content_type()?.ok_or_else(|| {
            anyhow!(
                "Expected content type `{}`, but no `Content-Type` header was found",
                expected
            )
        })?;
        if ct != *expected {
            bail!(
                "Expected content type `{}`, but instead found `{}`",
                expected,
                ct
            )
        }
        Ok(())
    }

    pub fn content_type(&self) -> Result<Option<Mime>> {
        self.headers.content_type()
    }

    pub fn get<T>(&self, name: impl AsRef<str>) -> Result<T>
    where
        T: FromStr,
        T::Err: Error + Send + Sync + 'static,
    {
        let name = name.as_ref();
        self.headers
            .parsed(name)?
            .ok_or_else(|| anyhow!("Expect header `{}`", name))
    }

    pub fn write(&self, writer: &mut dyn Write) -> Result<()> {
        self.headers.write(writer)?;
        if let Some(content) = &self.content {
            // TODO: how should we handle Content-Type and Content-Length, should they be implicitly
            // included in the headers, and how can we make sure of that?
            content.write(writer)?;
        }
        Ok(())
    }
}

#[derive(Clone, Default, Debug)]
pub struct Headers(pub Vec<Header>);

/// Convenience functions.
impl Headers {
    pub fn content(&self) -> Result<Option<(Mime, usize)>> {
        let ty = self.content_type()?;
        let len = self.content_length()?;
        match (ty, len) {
            (_, None) => Ok(None),
            (None, Some(_)) => bail!("Seen `Content-Length` without a `Content-Type`"),
            (Some(ty), Some(len)) => Ok(Some((ty, len))),
        }
    }

    pub fn content_type(&self) -> Result<Option<Mime>> {
        self.parsed("Content-Type")
    }

    pub fn content_length(&self) -> Result<Option<usize>> {
        self.parsed("Content-Length")
    }

    /// Returns a parsed value.
    ///
    /// Returns `Ok(None)` if the header was not found. Returns an error if the parsing failed.
    pub fn parsed<T>(&self, name: impl AsRef<str>) -> Result<Option<T>>
    where
        T: FromStr,
        T::Err: Error + Send + Sync + 'static,
    {
        let name = name.as_ref();
        if let Some(value) = self.value(name) {
            Ok(Some(value.parse::<T>()?))
        } else {
            Ok(None)
        }
    }

    /// Write headers to a binary stream.
    pub fn write(&self, writer: &mut dyn Write) -> Result<()> {
        for header in &self.0 {
            header.write(writer)?;
            writer.write_all(&[LF])?;
        }
        writer.write_all(&[LF])?;
        Ok(())
    }
}

/// Essential functions.
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
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Header {
    pub name: String,
    pub value: String,
}

impl Header {
    pub fn write(&self, writer: &mut dyn Write) -> Result<()> {
        writer.write_all(self.name.as_bytes())?;
        writer.write_all(": ".as_bytes())?;
        writer.write_all(self.value.as_bytes())?;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Content {
    pub ty: Mime,
    pub data: Vec<u8>,
}

impl Content {
    pub fn write(&self, writer: &mut dyn Write) -> Result<()> {
        writer.write_all(&self.data)?;
        Ok(())
    }
}
