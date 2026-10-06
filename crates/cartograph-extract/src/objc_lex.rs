//! Shared, constant-step Objective-C++ literal/comment rules for textual passes.

const RAW_PREFIXES: [&[u8]; 5] = [b"u8R\"", b"uR\"", b"UR\"", b"LR\"", b"R\""];
const MAX_RAW_DELIMITER_BYTES: usize = 16;
const COMMENT_DELIMITER_BYTES: usize = 2;
const ESCAPED_BYTES: usize = 2;
const CRLF_SPLICE_BYTES: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Region {
    Code,
    Comment,
    Literal,
}

#[derive(Clone, Copy, Default)]
enum Lexeme {
    #[default]
    Code,
    LineComment,
    BlockComment,
    Literal(u8),
    RawString {
        start: usize,
        length: usize,
    },
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Token {
    #[default]
    None,
    Word,
    Number,
}

#[derive(Default)]
pub(crate) struct ObjcLexer {
    lexeme: Lexeme,
    token: Token,
}

impl ObjcLexer {
    /// Consume one bounded lexical step; callers poll cancellation between steps.
    pub(crate) fn step(&mut self, bytes: &[u8], index: usize) -> (Region, usize) {
        match self.lexeme {
            Lexeme::Code => self.code_step(bytes, index),
            Lexeme::LineComment => self.line_comment_step(bytes, index),
            Lexeme::BlockComment => self.block_comment_step(bytes, index),
            Lexeme::Literal(_) => self.literal_step(bytes, index),
            Lexeme::RawString { .. } => self.raw_string_step(bytes, index),
        }
    }

    pub(crate) fn reset_token(&mut self) {
        self.token = Token::None;
    }

    fn code_step(&mut self, bytes: &[u8], index: usize) -> (Region, usize) {
        let Some(&byte) = bytes.get(index) else {
            return (Region::Code, index + 1);
        };
        if self.token == Token::None
            && let Some((start, length)) = raw_delimiter(bytes, index)
        {
            self.lexeme = Lexeme::RawString { start, length };
            return (Region::Literal, start + length + 1);
        }
        if let Some(lexeme) = comment_opener(&bytes[index..]) {
            self.lexeme = lexeme;
            self.token = Token::None;
            return (Region::Comment, index + COMMENT_DELIMITER_BYTES);
        }
        let separator = byte == b'\'' && self.token == Token::Number;
        if matches!(byte, b'\'' | b'"') && !separator {
            self.lexeme = Lexeme::Literal(byte);
            self.token = Token::None;
            return (Region::Literal, index + 1);
        }
        self.token = next_token(self.token, byte);
        (Region::Code, index + 1)
    }

    fn line_comment_step(&mut self, bytes: &[u8], index: usize) -> (Region, usize) {
        if bytes[index..].starts_with(b"\\\n") || bytes[index..].starts_with(b"\\\r\n") {
            return (Region::Comment, escaped_end(bytes, index));
        }
        if matches!(bytes.get(index), Some(b'\n' | b'\r')) {
            self.lexeme = Lexeme::Code;
        }
        (Region::Comment, index + 1)
    }

    fn block_comment_step(&mut self, bytes: &[u8], index: usize) -> (Region, usize) {
        if bytes[index..].starts_with(b"*/") {
            self.lexeme = Lexeme::Code;
            return (Region::Comment, index + COMMENT_DELIMITER_BYTES);
        }
        (Region::Comment, index + 1)
    }

    fn literal_step(&mut self, bytes: &[u8], index: usize) -> (Region, usize) {
        if bytes.get(index) == Some(&b'\\') {
            return (Region::Literal, escaped_end(bytes, index));
        }
        let Lexeme::Literal(quote) = self.lexeme else {
            return (Region::Code, index + 1);
        };
        if bytes.get(index) == Some(&quote) || matches!(bytes.get(index), Some(b'\n' | b'\r')) {
            self.lexeme = Lexeme::Code;
        }
        (Region::Literal, index + 1)
    }

    fn raw_string_step(&mut self, bytes: &[u8], index: usize) -> (Region, usize) {
        let Lexeme::RawString { start, length } = self.lexeme else {
            return (Region::Code, index + 1);
        };
        let end = index + length + 1;
        if bytes.get(index) == Some(&b')')
            && bytes.get(index + 1..end) == bytes.get(start..start + length)
            && bytes.get(end) == Some(&b'"')
        {
            self.lexeme = Lexeme::Code;
            return (Region::Literal, end + 1);
        }
        (Region::Literal, index + 1)
    }
}

fn escaped_end(bytes: &[u8], index: usize) -> usize {
    let length = if bytes[index..].starts_with(b"\\\r\n") {
        CRLF_SPLICE_BYTES
    } else {
        ESCAPED_BYTES
    };
    (index + length).min(bytes.len())
}

fn comment_opener(bytes: &[u8]) -> Option<Lexeme> {
    if bytes.starts_with(b"//") {
        Some(Lexeme::LineComment)
    } else if bytes.starts_with(b"/*") {
        Some(Lexeme::BlockComment)
    } else {
        None
    }
}

fn next_token(token: Token, byte: u8) -> Token {
    if byte.is_ascii_alphanumeric() || byte == b'_' {
        match token {
            Token::None if byte.is_ascii_digit() => Token::Number,
            Token::None => Token::Word,
            current => current,
        }
    } else if byte == b'\'' && token == Token::Number {
        Token::Number
    } else {
        Token::None
    }
}

fn raw_delimiter(bytes: &[u8], index: usize) -> Option<(usize, usize)> {
    let prefix = RAW_PREFIXES
        .iter()
        .find(|prefix| bytes[index..].starts_with(prefix))?;
    let start = index + prefix.len();
    for length in 0..=MAX_RAW_DELIMITER_BYTES {
        let byte = *bytes.get(start + length)?;
        if byte == b'(' {
            return Some((start, length));
        }
        if byte.is_ascii_whitespace() || matches!(byte, b')' | b'\\' | b'"') {
            return None;
        }
    }
    None
}
