//! Span-preserving pre-parse rewrite of React Native Objective-C export macros.
//!
//! `RCT_EXPORT_METHOD(sel…)`, `RCT_REMAP_METHOD(js, sel…)`,
//! `RCT_EXPORT_BLOCKING_SYNCHRONOUS_METHOD(sel…)`, and
//! `RCT_REMAP_BLOCKING_SYNCHRONOUS_METHOD(js, type, sel…)` expand to ordinary
//! Objective-C methods, but the grammar has no preprocessor: it parses each as a
//! macro type followed by an error cascade that drops the method, breaks the
//! enclosing `@implementation`, and turns a body call into a spurious function.
//!
//! The rewrite overwrites only the macro head (`NAME(` becomes a `- (void)` or
//! `- (id)` skeleton padded with spaces), the REMAP leading arguments, and the
//! closing parenthesis, so the parser sees `- (void) sel… { … }`. Every byte keeps
//! its offset and every newline stays a newline, so byte spans, lines, and
//! columns of the parsed tree are exact for the original source, which the
//! walker continues to read. The JS-visible name and export tagging are
//! recovered separately by the framework bridge from the original text.
//!
//! Unlike the v1 extractor's textual match, the scan skips comments and string
//! and character literals: a macro name or parenthesis inside one neither opens
//! nor closes an invocation, so a commented-out macro can never blank live code
//! and comment, docstring, and literal text always reaches the walker unchanged.

use crate::{
    ExtractError,
    objc_lex::{ObjcLexer, Region},
};

/// Bytes scanned between cancellation probes.
const CANCELLATION_INTERVAL_BYTES: usize = 64 * 1024;
/// Macro invocations rewritten between cancellation probes.
const CANCELLATION_INTERVAL_INVOCATIONS: usize = 256;

/// Fast gate shared by every export macro name.
const MACRO_PREFIX: &str = "RCT_";
/// One React Native method-export macro.
struct MacroSpec {
    /// Macro identifier as written in source.
    name: &'static str,
    /// Method-declaration prefix written over the macro name and `(`.
    skeleton: &'static str,
    /// Top-level arguments blanked before the native selector.
    leading_arguments: usize,
}

/// Names never prefix-shadow each other once the trailing `(` is required.
const MACROS: [MacroSpec; 4] = [
    MacroSpec {
        name: "RCT_REMAP_BLOCKING_SYNCHRONOUS_METHOD",
        skeleton: "- (id)",
        leading_arguments: 2,
    },
    MacroSpec {
        name: "RCT_EXPORT_BLOCKING_SYNCHRONOUS_METHOD",
        skeleton: "- (id)",
        leading_arguments: 0,
    },
    MacroSpec {
        name: "RCT_REMAP_METHOD",
        skeleton: "- (void)",
        leading_arguments: 1,
    },
    MacroSpec {
        name: "RCT_EXPORT_METHOD",
        skeleton: "- (void)",
        leading_arguments: 0,
    },
];

/// One balanced macro invocation: `NAME` at `start`, `(` at `open`, `)` at `close`.
#[derive(Clone, Copy)]
struct Invocation {
    start: usize,
    open: usize,
    close: usize,
    spec: &'static MacroSpec,
}

/// A macro whose `(` has been seen but whose closing `)` has not.
struct PendingInvocation {
    start: usize,
    open: usize,
    depth: isize,
    spec: &'static MacroSpec,
}

/// Return parseable Objective-C text when `source` contains export macros.
///
/// Only whole ASCII-delimited ranges are overwritten with ASCII, so the result
/// is valid UTF-8 with every character boundary at its original offset.
///
/// # Errors
///
/// Returns [`ExtractError::Cancelled`] when the probe requests cancellation and
/// [`ExtractError::OutputLimit`] when the rewritten copy cannot be allocated.
pub(crate) fn rewrite_react_native_macros(
    source: &str,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Option<String>, ExtractError> {
    if !source.contains(MACRO_PREFIX) {
        return Ok(None);
    }
    let bytes = source.as_bytes();
    let invocations = balanced_invocations(bytes, cancelled)?;
    if invocations.is_empty() {
        return Ok(None);
    }
    let mut rewritten = Vec::new();
    rewritten
        .try_reserve_exact(bytes.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    rewritten.extend_from_slice(bytes);
    let mut next_allowed = 0;
    for (position, invocation) in invocations.into_iter().enumerate() {
        if position.is_multiple_of(CANCELLATION_INTERVAL_INVOCATIONS) && cancelled() {
            return Err(ExtractError::Cancelled);
        }
        // A macro inside a rewritten macro's arguments is left untouched.
        if invocation.start < next_allowed {
            continue;
        }
        rewrite_invocation(&mut rewritten, invocation);
        next_allowed = invocation.close.saturating_add(1);
    }
    // Unreachable for valid input; an invalid rewrite falls back to the original text.
    Ok(String::from_utf8(rewritten).ok())
}

/// Every macro whose `(` has a matching `)`, in source order, found in one pass
/// counting parenthesis depth outside comments and literals (unbalanced macros
/// are skipped).
fn balanced_invocations(
    bytes: &[u8],
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Vec<Invocation>, ExtractError> {
    let mut scanner = InvocationScanner::default();
    let mut index = 0;
    let mut next_probe = 0;
    while index < bytes.len() {
        // Macro matches skip ahead, so probe on crossing a boundary, not on landing on one.
        if index >= next_probe {
            if cancelled() {
                return Err(ExtractError::Cancelled);
            }
            next_probe = index.saturating_add(CANCELLATION_INTERVAL_BYTES);
        }
        index = scanner.step(bytes, index)?;
    }
    if cancelled() {
        return Err(ExtractError::Cancelled);
    }
    let mut found = scanner.found;
    found.sort_unstable_by_key(|invocation| invocation.start);
    Ok(found)
}

/// Pairs each macro's `(` with the `)` that returns to the depth it opened at.
#[derive(Default)]
struct InvocationScanner {
    lexer: ObjcLexer,
    depth: isize,
    open: Vec<PendingInvocation>,
    found: Vec<Invocation>,
}

impl InvocationScanner {
    /// Only code bytes can open macros or change parenthesis depth.
    fn step(&mut self, bytes: &[u8], index: usize) -> Result<usize, ExtractError> {
        let (region, next) = self.lexer.step(bytes, index);
        if region != Region::Code {
            return Ok(next);
        }
        match bytes.get(index) {
            Some(b'R') => match macro_at(bytes, index) {
                Some(spec) => {
                    self.lexer.reset_token();
                    self.open_macro(index, spec)
                }
                None => Ok(next),
            },
            Some(b'(') => {
                self.depth = self.depth.saturating_add(1);
                Ok(next)
            }
            Some(b')') => {
                self.close_paren(index)?;
                Ok(next)
            }
            _ => Ok(next),
        }
    }

    /// Record a macro named at `start` and return the index after its `(`.
    fn open_macro(
        &mut self,
        start: usize,
        spec: &'static MacroSpec,
    ) -> Result<usize, ExtractError> {
        let open = start.saturating_add(spec.name.len());
        reserve_one(&mut self.open)?;
        self.open.push(PendingInvocation {
            start,
            open,
            depth: self.depth,
            spec,
        });
        self.depth = self.depth.saturating_add(1);
        Ok(open.saturating_add(1))
    }

    fn close_paren(&mut self, close: usize) -> Result<(), ExtractError> {
        self.depth = self.depth.saturating_sub(1);
        let depth = self.depth;
        let Some(pending) = self.open.pop_if(|pending| pending.depth == depth) else {
            return Ok(());
        };
        reserve_one(&mut self.found)?;
        self.found.push(Invocation {
            start: pending.start,
            open: pending.open,
            close,
            spec: pending.spec,
        });
        Ok(())
    }
}

fn reserve_one<T>(values: &mut Vec<T>) -> Result<(), ExtractError> {
    values.try_reserve(1).map_err(|_| ExtractError::OutputLimit)
}

/// The macro named at `index`, preceded by an identifier boundary and followed
/// immediately by `(`.
fn macro_at(bytes: &[u8], index: usize) -> Option<&'static MacroSpec> {
    if index
        .checked_sub(1)
        .and_then(|previous| bytes.get(previous))
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        return None;
    }
    let tail = bytes.get(index..)?;
    MACROS.iter().find(|spec| {
        tail.starts_with(spec.name.as_bytes()) && tail.get(spec.name.len()) == Some(&b'(')
    })
}

fn rewrite_invocation(bytes: &mut [u8], invocation: Invocation) {
    blank(bytes, invocation.start, invocation.open);
    let skeleton = invocation.spec.skeleton.as_bytes();
    if let Some(head) = bytes.get_mut(invocation.start..invocation.start + skeleton.len()) {
        head.copy_from_slice(skeleton);
    }
    if invocation.spec.leading_arguments > 0
        && let Some(comma) = nth_top_level_comma(bytes, invocation)
    {
        blank(bytes, invocation.open.saturating_add(1), comma);
    }
    blank(bytes, invocation.close, invocation.close);
}

/// The comma ending the REMAP leading arguments inside the macro's own
/// parentheses. `<>` nests like the other brackets so a generic return type
/// (`NSDictionary<NSString *, id> *`) keeps its inner comma. Comments and
/// literals can contain commas and brackets but never separate arguments.
fn nth_top_level_comma(bytes: &[u8], invocation: Invocation) -> Option<usize> {
    let mut depth = 0_usize;
    let mut found = 0_usize;
    let arguments = bytes.get(invocation.open.saturating_add(1)..invocation.close)?;
    let mut lexer = ObjcLexer::default();
    let mut offset = 0;
    while offset < arguments.len() {
        let (region, next) = lexer.step(arguments, offset);
        let index = offset;
        offset = next;
        if region != Region::Code {
            continue;
        }
        match arguments[index] {
            b'(' | b'[' | b'{' | b'<' => depth = depth.saturating_add(1),
            b')' | b']' | b'}' | b'>' => depth = depth.checked_sub(1)?,
            b',' if depth == 0 => {
                found = found.saturating_add(1);
                if found == invocation.spec.leading_arguments {
                    return Some(invocation.open.saturating_add(1).saturating_add(index));
                }
            }
            _ => {}
        }
    }
    None
}

/// Overwrite `start..=end` with spaces, keeping line breaks so lines stay exact.
fn blank(bytes: &mut [u8], start: usize, end: usize) {
    let Some(range) = bytes.get_mut(start..=end) else {
        return;
    };
    for byte in range {
        if !matches!(byte, b'\n' | b'\r') {
            *byte = b' ';
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewrite(source: &str) -> Option<String> {
        rewrite_react_native_macros(source, &mut || false)
            .unwrap_or_else(|error| panic!("rewrite failed: {error}"))
    }

    #[test]
    fn every_macro_shape_keeps_byte_length_and_line_breaks() {
        for source in [
            "RCT_EXPORT_METHOD(doSomething:(NSString *)name resolver:(RCTPromiseResolveBlock)resolve)\n{\n}",
            "RCT_REMAP_METHOD(getThing, getThingWithResolver:(RCTPromiseResolveBlock)resolve)\n{\n}",
            "RCT_EXPORT_BLOCKING_SYNCHRONOUS_METHOD(getName)\n{\n  return @\"x\";\n}",
            "RCT_REMAP_BLOCKING_SYNCHRONOUS_METHOD(getCount, NSNumber *, getCountValue)\n{\n}",
            "RCT_REMAP_BLOCKING_SYNCHRONOUS_METHOD(getMap, NSDictionary<NSString *, id> *, getMapValue)\n{\n}",
            "RCT_EXPORT_METHOD(f:(NSDictionary<NSString *, id> *)opts)\n{\n}",
            "RCT_REMAP_METHOD(\r\n  js\u{e9}Name,\r\n  native:(id)value)\r\n{\r\n}",
        ] {
            let rewritten = rewrite(source).unwrap_or_else(|| panic!("not rewritten: {source}"));
            assert_eq!(rewritten.len(), source.len(), "{source}");
            let breaks = |text: &str| {
                text.bytes()
                    .enumerate()
                    .filter(|(_, byte)| matches!(byte, b'\n' | b'\r'))
                    .map(|(offset, _)| offset)
                    .collect::<Vec<_>>()
            };
            assert_eq!(breaks(&rewritten), breaks(source), "{source}");
            assert!(!rewritten.contains("RCT_"), "{rewritten}");
            assert!(rewritten.starts_with("- ("), "{rewritten}");
        }
    }

    #[test]
    fn remap_arguments_are_blanked_up_to_the_native_selector() {
        assert_eq!(
            rewrite("RCT_REMAP_METHOD(getThing, getThingWithResolver:(id)r) {}").as_deref(),
            Some("- (void)                   getThingWithResolver:(id)r  {}")
        );
        let generic_return = "RCT_REMAP_BLOCKING_SYNCHRONOUS_METHOD(getMap, NSDictionary<NSString *, id> *, getMapValue) {}";
        let padding = generic_return.len() - "- (id)".len() - "getMapValue  {}".len();
        assert_eq!(
            rewrite(generic_return),
            Some(format!("- (id){}getMapValue  {{}}", " ".repeat(padding)))
        );
        assert_eq!(
            rewrite("RCT_EXPORT_BLOCKING_SYNCHRONOUS_METHOD(syncName) {}").as_deref(),
            Some("- (id)                                 syncName  {}")
        );
    }

    #[test]
    fn remap_separators_ignore_literals_and_comment_brackets() {
        for leading in [
            "jsName /* , > ] } */",
            "@\"js, > ] }\"",
            "'>'",
            "R\"tag(js, > ] })tag\"",
        ] {
            let source = format!("RCT_REMAP_METHOD({leading}, nativeMethod:(id)x) {{}}");
            let rewritten = rewrite(&source).unwrap_or_else(|| panic!("not rewritten"));
            assert_eq!(rewritten.len(), source.len());
            assert!(rewritten.ends_with("nativeMethod:(id)x  {}"), "{rewritten}");
            assert!(
                rewritten["- (void)".len()..source.find("nativeMethod").unwrap_or(0)]
                    .bytes()
                    .all(|byte| byte == b' ')
            );
        }
    }

    #[test]
    fn sources_without_a_balanced_macro_are_not_copied() {
        for source in [
            "",
            "@implementation Foo\n- (void)greet { NSLog(@\"hi\"); }\n@end",
            "int MY_RCT_EXPORT_METHOD_COUNT = 1;",
            "RCT_EXPORT_METHOD (spaced:(id)x) {}",
            "RCT_EXPORT_METHOD(unbalanced:(id)x {",
            "RCT_EXPORT_MODULE(Name)",
        ] {
            assert_eq!(rewrite(source), None, "{source}");
        }
    }

    #[test]
    fn a_multibyte_character_before_the_macro_keeps_the_selector() {
        let source = "// log \u{1f525} marker\nRCT_EXPORT_METHOD(getValue:(NSString *)key) {\n}";
        let rewritten = rewrite(source).unwrap_or_else(|| panic!("not rewritten"));
        assert_eq!(rewritten.len(), source.len());
        assert!(rewritten.contains("\u{1f525}"));
        assert!(rewritten.contains("- (void)          getValue:(NSString *)key  {"));
    }

    #[test]
    fn nested_macros_inside_rewritten_arguments_stay_untouched_and_later_ones_rewrite() {
        let source =
            "RCT_REMAP_METHOD(a, b:(id)RCT_EXPORT_METHOD(c)) {}\nRCT_EXPORT_METHOD(next) {}";
        let rewritten = rewrite(source).unwrap_or_else(|| panic!("not rewritten"));
        assert_eq!(rewritten.len(), source.len());
        assert!(rewritten.contains("RCT_EXPORT_METHOD(c)"), "{rewritten}");
        assert!(
            rewritten.ends_with("- (void)          next  {}"),
            "{rewritten}"
        );
    }

    #[test]
    fn an_unbalanced_macro_does_not_hide_a_later_balanced_one() {
        let source = "RCT_EXPORT_METHOD(open:(id)x {\n}\nRCT_EXPORT_METHOD(closed) {}";
        let rewritten = rewrite(source).unwrap_or_else(|| panic!("not rewritten"));
        assert!(
            rewritten.starts_with("RCT_EXPORT_METHOD(open:"),
            "{rewritten}"
        );
        assert!(
            rewritten.ends_with("- (void)          closed  {}"),
            "{rewritten}"
        );
    }

    #[test]
    fn a_malformed_remap_keeps_its_arguments_and_a_macro_inside_an_unbalanced_one_rewrites() {
        assert_eq!(
            rewrite("RCT_REMAP_METHOD(onlySelector:(id)x) {}").as_deref(),
            Some("- (void)         onlySelector:(id)x  {}")
        );
        let source = "RCT_EXPORT_METHOD(open:(id)x {\n  RCT_EXPORT_METHOD(inner) {}\n";
        let rewritten = rewrite(source).unwrap_or_else(|| panic!("not rewritten"));
        assert!(
            rewritten.starts_with("RCT_EXPORT_METHOD(open:"),
            "{rewritten}"
        );
        assert!(
            rewritten.contains("- (void)          inner  {}"),
            "{rewritten}"
        );
    }

    #[test]
    fn comments_and_literals_never_open_or_close_a_macro() {
        for inert in [
            // A commented-out macro whose `)` sits in a later comment must not
            // blank the live code between them.
            "// RCT_REMAP_METHOD(\n@implementation A\nint a, b;\n- (void)run {}\n// )\n@end\n",
            "/* RCT_EXPORT_METHOD(x) */ NSString *s = @\"RCT_EXPORT_METHOD(y)\";",
            "char c = 'R'; NSString *t = @\"\\\"RCT_EXPORT_METHOD(z)\";",
        ] {
            assert_eq!(rewrite(inert), None, "{inert}");
        }
        let source = "RCT_EXPORT_METHOD(a:(id)x /* ) */ b:(NSString *)y // )\n) {}";
        let rewritten = rewrite(source).unwrap_or_else(|| panic!("not rewritten"));
        assert_eq!(rewritten.len(), source.len());
        assert_eq!(
            rewritten,
            "- (void)          a:(id)x /* ) */ b:(NSString *)y // )\n  {}"
        );
        let literal = "RCT_EXPORT_METHOD(f:(char)c) { if (c == '(') {} }\nRCT_EXPORT_METHOD(g) {}";
        let rewritten = rewrite(literal).unwrap_or_else(|| panic!("not rewritten"));
        assert!(
            rewritten.ends_with("- (void)          g  {}"),
            "{rewritten}"
        );
    }

    #[test]
    fn digit_separators_are_not_character_literals() {
        let source = "- (int)count { return 1'000 + 0xFF'FF'FF; } RCT_EXPORT_METHOD(run) {}";
        let rewritten = rewrite(source).unwrap_or_else(|| panic!("not rewritten"));
        assert!(
            rewritten.ends_with("- (void)          run  {}"),
            "{rewritten}"
        );
        for literal in [
            "char c = L')';",
            "char c = u8'(';",
            "char c = ')';",
            "- (int)count { return'('; }",
            "size_t n = sizeof'(';",
        ] {
            let source = format!("{literal} RCT_EXPORT_METHOD(run) {{}}");
            let rewritten = rewrite(&source).unwrap_or_else(|| panic!("not rewritten: {source}"));
            assert!(rewritten.starts_with(literal), "{rewritten}");
            assert!(
                rewritten.ends_with("- (void)          run  {}"),
                "{rewritten}"
            );
        }
    }

    #[test]
    fn long_comments_and_literals_keep_the_cancellation_cadence() {
        let filler = "x".repeat(3 * CANCELLATION_INTERVAL_BYTES);
        for source in [
            format!("RCT_EXPORT_METHOD(a) {{}} /* {filler} */"),
            format!("RCT_EXPORT_METHOD(a) {{}} // {filler}\n"),
            format!("RCT_EXPORT_METHOD(a) {{}} @\"{filler}\";"),
        ] {
            let mut probes = 0_usize;
            let outcome = rewrite_react_native_macros(&source, &mut || {
                probes += 1;
                false
            });
            assert!(matches!(outcome, Ok(Some(_))));
            assert!(
                probes > source.len() / CANCELLATION_INTERVAL_BYTES,
                "{probes} probes for {} bytes",
                source.len()
            );
        }
    }

    #[test]
    fn cancellation_stops_the_scan() {
        let source = "RCT_EXPORT_METHOD(x) {}";
        assert_eq!(
            rewrite_react_native_macros(source, &mut || true),
            Err(ExtractError::Cancelled)
        );
    }

    #[test]
    fn macros_straddling_probe_boundaries_still_reach_later_probes() {
        let invocation = "RCT_EXPORT_METHOD(x) {}";
        let mut source = String::new();
        for boundary in 1..=3 {
            let start = boundary * CANCELLATION_INTERVAL_BYTES - 4;
            source.push_str(&" ".repeat(start - source.len()));
            source.push_str(invocation);
        }
        let mut probes = 0;
        let outcome = rewrite_react_native_macros(&source, &mut || {
            probes += 1;
            probes > 1
        });
        assert_eq!(outcome, Err(ExtractError::Cancelled));
    }
}
