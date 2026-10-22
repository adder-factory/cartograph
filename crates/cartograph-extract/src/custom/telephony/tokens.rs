//! Kamailio tokens borrow source offsets, including opaque string values.
use std::mem::size_of;

use super::super::poll_cancellation;
use super::{CustomBuilder, ExtractError};

const MAXIMUM_TOKENS: usize = 200_000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Word,
    String,
    Directive,
    Punctuation(u8),
}

#[derive(Clone, Copy)]
pub(super) struct Token {
    pub kind: Kind,
    pub start: usize,
    pub end: usize,
}

impl Token {
    pub fn text(self, source: &str) -> &str {
        &source[self.start..self.end]
    }
    pub fn value(self, source: &str) -> Option<&str> {
        match self.kind {
            Kind::Word => Some(self.text(source)),
            Kind::String => source
                .get(self.start + 1..self.end.checked_sub(1)?)
                .filter(|text| !text.contains(['\\', '\n', '\r'])),
            _ => None,
        }
    }

    pub fn keyword(self, source: &str) -> &str {
        if self.kind == Kind::Directive {
            &source[self.start + 2..self.end]
        } else {
            self.text(source)
        }
    }
}

pub(super) fn scan(
    builder: &mut CustomBuilder<'_, '_>,
) -> Result<(Vec<Token>, bool), ExtractError> {
    let mut cursor = TokenCursor {
        source: builder.source(),
        position: 0,
        next_poll: 0,
    };
    let mut output = Vec::new();
    while cursor.position < cursor.source.len() {
        cursor.poll(builder)?;
        let start = cursor.position;
        let kind = match cursor.read(builder)? {
            Scanned::Token(kind) => kind,
            Scanned::Skipped => continue,
            Scanned::Unterminated => return Ok((output, false)),
        };
        if output.len() >= MAXIMUM_TOKENS {
            return Err(ExtractError::OutputLimit);
        }
        builder.budget.reserve_working_bytes(
            u64::try_from(2 * size_of::<Token>()).map_err(|_| ExtractError::OutputLimit)?,
        )?;
        output.push(Token {
            kind,
            start,
            end: cursor.position,
        });
    }
    Ok((output, true))
}

enum Scanned {
    Token(Kind),
    Skipped,
    Unterminated,
}

struct TokenCursor<'source> {
    source: &'source str,
    position: usize,
    next_poll: usize,
}

impl TokenCursor<'_> {
    fn poll(&mut self, builder: &mut CustomBuilder<'_, '_>) -> Result<(), ExtractError> {
        poll_cancellation(builder.cancelled, self.position, &mut self.next_poll)
    }

    /// Consume at least one byte, or report an unterminated opaque region.
    fn read(&mut self, builder: &mut CustomBuilder<'_, '_>) -> Result<Scanned, ExtractError> {
        let bytes = self.source.as_bytes();
        let byte = bytes[self.position];
        if byte.is_ascii_whitespace() {
            self.position += 1;
            return Ok(Scanned::Skipped);
        }
        let tail = &bytes[self.position..];
        if tail.starts_with(b"/*") {
            return self.block_comment(builder);
        }
        let kind = if tail.starts_with(b"#!") || tail.starts_with(b"!!") {
            self.directive(builder)?;
            Kind::Directive
        } else if byte == b'#' || tail.starts_with(b"//") {
            self.line_comment(builder)?;
            return Ok(Scanned::Skipped);
        } else if matches!(byte, b'\'' | b'"') {
            return self.string(builder, byte);
        } else if byte.is_ascii_alphanumeric() || byte == b'_' {
            self.word(builder)?;
            Kind::Word
        } else {
            self.punctuation(byte)
        };
        Ok(Scanned::Token(kind))
    }

    fn punctuation(&mut self, byte: u8) -> Kind {
        // Non-ASCII is opaque, but never split a UTF-8 code point.
        self.position += if byte.is_ascii() {
            1
        } else {
            self.source[self.position..]
                .chars()
                .next()
                .map_or(1, char::len_utf8)
        };
        Kind::Punctuation(byte)
    }

    fn block_comment(
        &mut self,
        builder: &mut CustomBuilder<'_, '_>,
    ) -> Result<Scanned, ExtractError> {
        let bytes = self.source.as_bytes();
        self.position += 2;
        while self.position < bytes.len() && !bytes[self.position..].starts_with(b"*/") {
            self.poll(builder)?;
            self.position += 1;
        }
        if self.position == bytes.len() {
            return Ok(Scanned::Unterminated);
        }
        self.position += 2;
        Ok(Scanned::Skipped)
    }

    fn directive(&mut self, builder: &mut CustomBuilder<'_, '_>) -> Result<(), ExtractError> {
        let bytes = self.source.as_bytes();
        self.position += 2;
        while bytes
            .get(self.position)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            self.poll(builder)?;
            self.position += 1;
        }
        Ok(())
    }

    fn line_comment(&mut self, builder: &mut CustomBuilder<'_, '_>) -> Result<(), ExtractError> {
        let bytes = self.source.as_bytes();
        while self.position < bytes.len() && bytes[self.position] != b'\n' {
            self.poll(builder)?;
            self.position += 1;
        }
        Ok(())
    }

    fn string(
        &mut self,
        builder: &mut CustomBuilder<'_, '_>,
        quote: u8,
    ) -> Result<Scanned, ExtractError> {
        let bytes = self.source.as_bytes();
        self.position += 1;
        while self.position < bytes.len() && bytes[self.position] != quote {
            self.poll(builder)?;
            if bytes[self.position] == b'\\' {
                self.position += 1;
            }
            self.position = self.position.saturating_add(1).min(bytes.len());
        }
        if self.position == bytes.len() {
            return Ok(Scanned::Unterminated);
        }
        self.position += 1;
        Ok(Scanned::Token(Kind::String))
    }

    fn word(&mut self, builder: &mut CustomBuilder<'_, '_>) -> Result<(), ExtractError> {
        let bytes = self.source.as_bytes();
        while bytes
            .get(self.position)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || b"_:-.".contains(byte))
        {
            self.poll(builder)?;
            self.position += 1;
        }
        Ok(())
    }
}
