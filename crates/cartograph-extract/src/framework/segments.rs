//! Delimited top-level source slices with their original byte offsets.

use crate::code_scan::CodeScan;

pub(crate) struct Segments<'s> {
    text: &'s str,
    code: CodeScan<'s>,
    separator: u8,
    depth: usize,
    start: usize,
    finished: bool,
}

impl<'s> Segments<'s> {
    pub(crate) fn new(text: &'s str, separator: u8) -> Self {
        Self {
            text,
            code: CodeScan::new(text.as_bytes()),
            separator,
            depth: 0,
            start: 0,
            finished: false,
        }
    }
}

impl<'s> Iterator for Segments<'s> {
    type Item = (usize, &'s str);

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        for (index, byte) in self.code.by_ref() {
            match byte {
                b'(' | b'{' | b'[' => self.depth = self.depth.saturating_add(1),
                b')' | b'}' | b']' => self.depth = self.depth.saturating_sub(1),
                _ => {}
            }
            if byte == self.separator && self.depth == 0 {
                let start = self.start;
                self.start = index + 1;
                return Some((start, &self.text[start..index]));
            }
        }
        self.finished = true;
        Some((self.start, &self.text[self.start..]))
    }
}
