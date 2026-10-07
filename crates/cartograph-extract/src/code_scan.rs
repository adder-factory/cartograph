//! Byte-level scan of C-family source text that yields only code bytes.
//!
//! Text scanners that look for structural punctuation (a class body brace, the
//! start of a line) must not read it from string or character literals or from
//! comments. [`CodeScan`] skips `"…"` and `'…'` literals (with backslash
//! escapes), `//` line comments and `/* … */` block comments, and optionally
//! re-enters code inside Groovy `"${…}"` interpolations. It allocates nothing
//! and visits each byte once, so callers bound it by bounding their input.

/// Lexical region of the byte being scanned.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Region {
    /// Code; bytes are yielded.
    Code,
    /// Inside a literal opened by this quote byte.
    Quote(u8),
    /// After a backslash inside a literal opened by this quote byte.
    Escape(u8),
    /// Inside a `//` comment, until the end of the line.
    LineComment,
    /// Inside a `/* … */` comment.
    BlockComment,
}

/// Iterator over the `(offset, byte)` pairs of `text` that are code.
pub(crate) struct CodeScan<'text> {
    /// Text being scanned.
    text: &'text [u8],
    /// Offset of the next byte to examine.
    index: usize,
    /// Region of the next byte.
    region: Region,
    /// Whether a double-quoted literal re-enters code at `${`.
    interpolation: bool,
    /// Brace depth of the interpolation being scanned as code; zero outside one.
    interpolation_depth: usize,
}

impl<'text> CodeScan<'text> {
    /// Scan `text` from its first byte, which must start in code.
    pub(crate) const fn new(text: &'text [u8]) -> Self {
        Self {
            text,
            index: 0,
            region: Region::Code,
            interpolation: false,
            interpolation_depth: 0,
        }
    }

    /// Treat `${…}` inside double-quoted literals as code (Groovy `GString`s).
    pub(crate) const fn with_interpolation(mut self) -> Self {
        self.interpolation = true;
        self
    }

    /// Whether the scan has stopped inside a literal or comment.
    pub(crate) fn in_literal(&self) -> bool {
        self.region != Region::Code
    }

    /// Byte at `offset`, if any.
    fn byte_at(&self, offset: usize) -> Option<u8> {
        self.text.get(offset).copied()
    }

    /// Advance past one non-code byte at `index`; the newline that ends a line
    /// comment is left to be scanned as code.
    fn skip_literal_byte(&mut self, byte: u8) {
        if self.region == Region::LineComment && byte == b'\n' {
            self.region = Region::Code;
            return;
        }
        let next = self.byte_at(self.index + 1);
        self.region = match self.region {
            Region::Quote(quote) if byte == b'\\' => Region::Escape(quote),
            Region::Quote(b'"') if self.interpolation && byte == b'$' && next == Some(b'{') => {
                self.index += 1;
                self.interpolation_depth = 1;
                Region::Code
            }
            Region::Quote(quote) if byte == quote => Region::Code,
            Region::Escape(quote) => Region::Quote(quote),
            Region::BlockComment if byte == b'*' && next == Some(b'/') => {
                self.index += 1;
                Region::Code
            }
            region => region,
        };
        self.index += 1;
    }

    /// Classify one code byte at `index`; `true` when it is yielded as code.
    fn enter_code_byte(&mut self, byte: u8) -> bool {
        let next = self.byte_at(self.index + 1);
        let entered = match byte {
            b'"' | b'\'' => Some(Region::Quote(byte)),
            b'/' if next == Some(b'/') => Some(Region::LineComment),
            b'/' if next == Some(b'*') => Some(Region::BlockComment),
            _ => None,
        };
        if let Some(region) = entered {
            self.region = region;
            self.index += if matches!(region, Region::Quote(_)) {
                1
            } else {
                2
            };
            return false;
        }
        if self.interpolation_depth > 0 {
            if byte == b'{' {
                self.interpolation_depth += 1;
            } else if byte == b'}' {
                self.interpolation_depth -= 1;
                if self.interpolation_depth == 0 {
                    self.region = Region::Quote(b'"');
                    self.index += 1;
                    return false;
                }
            }
        }
        self.index += 1;
        true
    }

    /// Scan up to a byte boundary, retaining lexical state for the next chunk.
    pub(crate) fn next_bounded(&mut self, boundary: usize) -> Option<(usize, u8)> {
        while self.index < boundary {
            let byte = self.byte_at(self.index)?;
            let offset = self.index;
            if self.region != Region::Code {
                self.skip_literal_byte(byte);
            } else if self.enter_code_byte(byte) {
                return Some((offset, byte));
            }
        }
        None
    }
}

impl Iterator for CodeScan<'_> {
    type Item = (usize, u8);

    fn next(&mut self) -> Option<Self::Item> {
        self.next_bounded(self.text.len())
    }
}

#[cfg(test)]
mod tests {
    use super::CodeScan;

    fn code(text: &str) -> String {
        CodeScan::new(text.as_bytes())
            .map(|(_, byte)| char::from(byte))
            .collect()
    }

    #[test]
    fn literals_and_comments_are_not_code() {
        assert_eq!(code(r#"a "{" b '}' c"#), "a  b  c");
        assert_eq!(code("a /* don't { */ b // '{\nc"), "a  b \nc");
        assert_eq!(code(r#"x "esc \" {" y"#), "x  y");
    }

    #[test]
    fn groovy_interpolation_is_code_and_unterminated_literals_are_reported() {
        let text = br#"s "a ${m(x)} b" t"#;
        let interpolated = CodeScan::new(text)
            .with_interpolation()
            .map(|(_, byte)| char::from(byte))
            .collect::<String>();
        assert_eq!(interpolated, "s m(x) t");
        let mut open = CodeScan::new(br#"def "uses a.b"#);
        open.by_ref().for_each(drop);
        assert!(open.in_literal());
        let mut comment = CodeScan::new(b"x /* still");
        comment.by_ref().for_each(drop);
        assert!(comment.in_literal());
    }
}
