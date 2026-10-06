//! Raw-text elements, comments, tags, and template expressions of a component
//! file.
//!
//! The file is scanned once like an HTML parser: `<script>` and `<style>`
//! content is raw text, so a `<` inside script code never starts a tag and
//! cannot hide `</script>`. Ordinary tags (with their quoted or braced
//! attribute values), HTML comments, and template expressions are skipped
//! whole, so `<script>` text inside them is never mistaken for code, and text
//! inside an expression or a comment (`{{ '<Fake />' }}`) is never a tag.

use crate::ExtractError;

use super::{
    super::{MarkupTag, parse_markup_tag, tag_attribute},
    template_scan::TemplateScanner,
};
use crate::walk::embedded_script::{ScriptDialect, ScriptRegion, script_type_is_code};

/// Opening and closing delimiters of an HTML comment.
const HTML_COMMENT_OPEN: &str = "<!--";
const HTML_COMMENT_CLOSE: &str = "-->";

/// Elements whose content is raw text rather than markup.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RawElementKind {
    Script,
    Style,
}

/// One `<script>` or `<style>` element of a component file.
#[derive(Clone, Copy)]
pub(super) struct RawElement<'source> {
    kind: RawElementKind,
    pub(super) opening: MarkupTag<'source>,
    content_start: usize,
    content_end: usize,
    pub(super) end: usize,
}

/// Raw-text elements, comments, and opening tags of one component file, in
/// source order.
pub(super) struct ComponentLayout<'source> {
    /// `<script>` and `<style>` elements.
    pub(super) elements: Vec<RawElement<'source>>,
    /// HTML comment ranges.
    comments: Vec<(usize, usize)>,
    /// Ordinary opening tags of the template markup.
    pub(super) tags: Vec<MarkupTag<'source>>,
}

impl ComponentLayout<'_> {
    /// Program regions of every executable `<script>` element; `lang` selects
    /// the dialect and data blocks such as `type="application/ld+json"` are
    /// skipped.
    pub(super) fn script_regions(&self) -> Vec<ScriptRegion> {
        self.elements
            .iter()
            .filter(|element| {
                element.kind == RawElementKind::Script
                    && element.content_start < element.content_end
                    && script_type_is_code(element_attribute(element.opening, "type"))
            })
            .map(|element| {
                ScriptRegion::program(
                    element.content_start,
                    element.content_end,
                    ScriptDialect::from_lang_attribute(element_attribute(element.opening, "lang")),
                )
            })
            .collect()
    }

    /// Ranges that hold no template markup: raw-text elements and comments.
    fn skipped_ranges(&self) -> Vec<(usize, usize)> {
        let mut ranges = self
            .elements
            .iter()
            .map(|element| (element.opening.start, element.end))
            .chain(self.comments.iter().copied())
            .collect::<Vec<_>>();
        ranges.sort_unstable();
        ranges
    }
}

/// Scan the raw-text elements, comments, and opening tags of a component file.
pub(super) fn component_layout<'source>(
    scanner: &mut TemplateScanner<'source, '_>,
) -> Result<ComponentLayout<'source>, ExtractError> {
    let source = scanner.source();
    let mut layout = ComponentLayout {
        elements: Vec::new(),
        comments: Vec::new(),
        tags: Vec::new(),
    };
    let mut cursor = 0;
    // Once a tag runs to the end of the file, no later ordinary tag can end
    // either; stop trying instead of rescanning the tail per tag.
    let mut ordinary_tags_end = true;
    while let Some(start) = scanner.markup_start(cursor)? {
        cursor = if source[start..].starts_with('{') {
            scanner.past_expression(start)?
        } else if source[start..].starts_with(HTML_COMMENT_OPEN) {
            let end = past_html_comment(scanner, start)?;
            layout.comments.push((start, end));
            end
        } else if let Some(kind) = raw_element_kind(&source[start + 1..]) {
            let Some(element) = raw_element(scanner, start, kind)? else {
                break;
            };
            layout.elements.push(element);
            element.end.max(start + 1)
        } else if ordinary_tags_end && starts_tag_name(source, start + 1) {
            let Some(end) = scanner.tag_end(start + 1)? else {
                ordinary_tags_end = false;
                cursor = start + 1;
                continue;
            };
            layout.tags.extend(parse_markup_tag(source, start, end));
            end + 1
        } else {
            start + 1
        };
    }
    Ok(layout)
}

/// Value of a raw element's attribute, quoted or unquoted (`<script lang=ts>`).
fn element_attribute<'source>(opening: MarkupTag<'source>, key: &str) -> Option<&'source str> {
    tag_attribute(opening, key)
        .map(|(_, value)| value)
        .or_else(|| unquoted_attribute(opening.raw, key))
}

/// Unquoted value of attribute `key` in a tag's raw text, ignoring text inside
/// quoted values. HTML ends an unquoted value at whitespace.
fn unquoted_attribute<'raw>(raw: &'raw str, key: &str) -> Option<&'raw str> {
    let bytes = raw.as_bytes();
    let mut quote = None;
    for (index, &byte) in bytes.iter().enumerate() {
        match quote {
            Some(open) if byte == open => quote = None,
            Some(_) => {}
            None if matches!(byte, b'"' | b'\'') => quote = Some(byte),
            None => {
                let after_space = index > 0 && bytes[index - 1].is_ascii_whitespace();
                if let Some(value) = after_space
                    .then(|| unquoted_value_at(raw, index, key))
                    .flatten()
                {
                    return Some(value);
                }
            }
        }
    }
    None
}

/// The unquoted value of `key=value` starting at `at`; HTML allows
/// whitespace around the `=` (`lang = ts`).
fn unquoted_value_at<'raw>(raw: &'raw str, at: usize, key: &str) -> Option<&'raw str> {
    let after_key = at.checked_add(key.len())?;
    let name = raw.get(at..after_key)?;
    if !name.eq_ignore_ascii_case(key) {
        return None;
    }
    let value = raw[after_key..]
        .trim_start()
        .strip_prefix('=')?
        .trim_start();
    if value.starts_with(['"', '\'']) {
        return None;
    }
    let end = value
        .find(|character: char| character.is_ascii_whitespace())
        .unwrap_or(value.len());
    Some(&value[..end])
}

fn starts_tag_name(source: &str, at: usize) -> bool {
    source
        .as_bytes()
        .get(at)
        .is_some_and(u8::is_ascii_alphabetic)
}

/// Offset after the HTML comment opening at `start` (EOF when unterminated).
fn past_html_comment(
    scanner: &mut TemplateScanner<'_, '_>,
    start: usize,
) -> Result<usize, ExtractError> {
    let body = start + HTML_COMMENT_OPEN.len();
    Ok(scanner
        .find_from(body, HTML_COMMENT_CLOSE.as_bytes())?
        .map_or(scanner.source().len(), |close| {
            close + HTML_COMMENT_CLOSE.len()
        }))
}

fn raw_element_kind(after_open: &str) -> Option<RawElementKind> {
    [
        ("script", RawElementKind::Script),
        ("style", RawElementKind::Style),
    ]
    .into_iter()
    .find_map(|(name, kind)| {
        let head = after_open.get(..name.len())?;
        (head.eq_ignore_ascii_case(name) && is_tag_name_boundary(&after_open[name.len()..]))
            .then_some(kind)
    })
}

/// Whether a tag name ends here: HTML ends one at whitespace, `/`, `>`, or EOF.
fn is_tag_name_boundary(rest: &str) -> bool {
    rest.as_bytes()
        .first()
        .is_none_or(|byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>'))
}

/// The raw-text element whose opening tag starts at `start`.
fn raw_element<'source>(
    scanner: &mut TemplateScanner<'source, '_>,
    start: usize,
    kind: RawElementKind,
) -> Result<Option<RawElement<'source>>, ExtractError> {
    let source = scanner.source();
    let Some(opening) = scanner
        .tag_end(start + 1)?
        .and_then(|end| parse_markup_tag(source, start, end))
    else {
        return Ok(None);
    };
    let content_start = opening.end;
    if opening.self_closing {
        return Ok(Some(RawElement {
            kind,
            opening,
            content_start,
            content_end: content_start,
            end: content_start,
        }));
    }
    let closing = match kind {
        RawElementKind::Script => "</script",
        RawElementKind::Style => "</style",
    };
    let content_end = raw_text_end(scanner, content_start, closing)?.unwrap_or(source.len());
    let end = scanner
        .find_from(content_end, b">")?
        .map_or(source.len(), |end| end + 1);
    Ok(Some(RawElement {
        kind,
        opening,
        content_start,
        content_end,
        end,
    }))
}

/// Offset of the end tag that closes raw text (`</script>` but not `</scripture>`).
fn raw_text_end(
    scanner: &mut TemplateScanner<'_, '_>,
    start: usize,
    closing: &str,
) -> Result<Option<usize>, ExtractError> {
    let mut from = start;
    while let Some(at) = scanner.find_case_insensitive(from, closing)? {
        if is_tag_name_boundary(&scanner.source()[at + closing.len()..]) {
            return Ok(Some(at));
        }
        from = at + 1;
    }
    Ok(None)
}

/// The layout of a component's template and the dialect of its expressions.
pub(super) struct TemplateSurface<'layout, 'source> {
    pub(super) layout: &'layout ComponentLayout<'source>,
    pub(super) dialect: ScriptDialect,
}

/// Template expressions (`{{ … }}` in Vue, `{ … }` in Svelte) outside raw-text
/// elements and comments. Block and directive tags (`{#if}`, `{/if}`,
/// `{:else}`, `{@html}`) are not expressions and are skipped.
pub(super) fn template_expression_regions(
    scanner: &mut TemplateScanner<'_, '_>,
    surface: &TemplateSurface<'_, '_>,
) -> Result<Vec<ScriptRegion>, ExtractError> {
    let source = scanner.source();
    let open = scanner.delimiters().open;
    let dialect = surface.dialect;
    let skipped = surface.layout.skipped_ranges();
    let mut skipped = skipped.iter().peekable();
    let mut regions = Vec::new();
    let mut cursor = 0;
    while let Some(open_at) = scanner.find_from(cursor, open.as_bytes())? {
        while skipped.next_if(|(_, end)| *end <= open_at).is_some() {}
        if let Some((_, end)) = skipped.peek().filter(|(start, _)| *start <= open_at) {
            cursor = *end;
            continue;
        }
        let content_start = open_at + open.len();
        let Some(close_at) = scanner.expression_close(content_start)? else {
            break;
        };
        let expression = source[content_start..close_at].trim_start();
        if !expression.is_empty() && !scanner.is_directive(expression)? {
            regions.push(ScriptRegion::template_expression(
                content_start,
                close_at,
                dialect,
            ));
        }
        cursor = close_at + scanner.delimiters().close.len();
    }
    Ok(regions)
}
