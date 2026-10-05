//! Liquid (Shopify) section schemas and partner names.
//!
//! `{% schema %}...{% endschema %}` holds a JSON document whose `name` labels
//! the section in the theme editor. The block declares one constant named by
//! that label; its JSON is literal configuration, so nothing inside it is
//! scanned for `{{ ... }}` references.

use std::{fmt, ops::Range};

use cartograph_domain::SymbolKind;
use serde::{
    Deserialize, Deserializer,
    de::{IgnoredAny, MapAccess, SeqAccess, Visitor},
};

use crate::ExtractError;

use super::{
    CustomBuilder, CustomSymbolInput, SourceSliceInput, SymbolOptions, first_identifier,
    looks_sensitive, poll_cancellation,
};

/// Longest Liquid schema or partner name retained as a declaration name.
const MAX_LIQUID_NAME_BYTES: usize = 256;
/// Name of a schema block whose JSON declares no usable `name`.
const DEFAULT_SCHEMA_NAME: &str = "schema";
/// Locale preferred when a schema name is a translation map.
const PREFERRED_LOCALE: &str = "en";

/// Complete schema blocks seen so far, in source order, and whether a search
/// for `{% endschema %}` already reached the end of the file.
#[derive(Default)]
pub(super) struct SchemaBlocks {
    ranges: Vec<Range<usize>>,
    unterminated: bool,
}

impl SchemaBlocks {
    /// Byte ranges of complete schema blocks, sorted and disjoint.
    pub(super) fn ranges(&self) -> &[Range<usize>] {
        &self.ranges
    }
}

/// Declare the constant of a complete `{% schema %}` block opened by `tag`
/// and return the byte after its `{% endschema %}`. Once one schema has no
/// closing tag, later ones cannot either, so the file is never rescanned.
pub(super) fn extract_schema(
    builder: &mut CustomBuilder<'_, '_>,
    tag: SourceSliceInput<'_>,
    blocks: &mut SchemaBlocks,
) -> Result<Option<usize>, ExtractError> {
    if blocks.unterminated
        || first_identifier(tag.value).map(|(_, command)| command) != Some("schema")
    {
        return Ok(None);
    }
    let source = builder.source();
    let Some((body_end, block_end)) = end_tag(builder, tag.end, "endschema")? else {
        blocks.unterminated = true;
        return Ok(None);
    };
    let name = schema_name(&source[tag.end..body_end]);
    let name = name.as_deref().unwrap_or(DEFAULT_SCHEMA_NAME);
    builder.add_symbol(
        CustomSymbolInput::new(
            SymbolKind::Constant,
            name,
            format!("{}::schema:{name}", builder.path()),
        )
        .at(tag.start, block_end)
        .with_options(SymbolOptions {
            body_search_text: format!("schema {name}"),
            ..SymbolOptions::default()
        }),
    )?;
    blocks.ranges.push(tag.start..block_end);
    Ok(Some(block_end))
}

/// Start and end of the first `{% <command> %}` tag at or after `from`.
fn end_tag(
    builder: &mut CustomBuilder<'_, '_>,
    from: usize,
    command: &str,
) -> Result<Option<(usize, usize)>, ExtractError> {
    let source = builder.source();
    let mut cursor = from;
    let mut next_poll = from;
    while let Some(relative) = source.get(cursor..).and_then(|rest| rest.find("{%")) {
        poll_cancellation(&mut *builder.cancelled, cursor, &mut next_poll)?;
        let open = cursor + relative;
        let Some(close) = source[open + 2..].find("%}").map(|end| open + 2 + end + 2) else {
            return Ok(None);
        };
        let raw = source[open + 2..close - 2]
            .trim_matches(|character: char| character.is_whitespace() || character == '-');
        if first_identifier(raw).is_some_and(|(_, found)| found == command) {
            return Ok(Some((open, close)));
        }
        cursor = close;
    }
    Ok(None)
}

/// The schema's display name: a non-empty `name` string, or for a locale map
/// its `en` entry, else its first non-empty entry in document order.
fn schema_name(body: &str) -> Option<String> {
    let body = body.trim();
    // Only a JSON object is a schema; a struct deserializer would also
    // accept a positional array.
    if !body.starts_with('{') {
        return None;
    }
    let head = serde_json::from_str::<SchemaHead>(body).ok()?;
    let name = match head.name? {
        SchemaName::Text(name) => Some(name),
        SchemaName::Locales(locales) => {
            let preferred = locales
                .iter()
                .find(|(locale, name)| locale == PREFERRED_LOCALE && name.is_some());
            preferred
                .or_else(|| locales.iter().find(|(_, name)| name.is_some()))
                .and_then(|(_, name)| name.clone())
        }
        SchemaName::Other => None,
    }?;
    is_display_name(&name).then_some(name)
}

/// A quoted Liquid name (`'hero card'`, `"Hero Banner"`): bounded, single
/// line, free of markup delimiters, and not credential-shaped.
pub(super) fn is_display_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_LIQUID_NAME_BYTES
        && !value
            .chars()
            .any(|character| character.is_control() || matches!(character, '{' | '}' | '%'))
        && !looks_sensitive(value)
}

/// Only the `name` member of a schema document; everything else is skipped
/// without being materialized.
#[derive(Deserialize)]
struct SchemaHead {
    #[serde(default)]
    name: Option<SchemaName>,
}

/// A schema `name`: plain text, a locale map kept in document order (entries
/// whose value is not a non-empty string are `None`), or anything else.
enum SchemaName {
    Text(String),
    Locales(Vec<(String, Option<String>)>),
    Other,
}

impl<'de> Deserialize<'de> for SchemaName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(SchemaNameVisitor)
    }
}

/// A trimmed, non-empty string, or `None`.
fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// A locale-map value: kept only when it is a non-empty string.
struct LocaleValue(Option<String>);

impl<'de> Deserialize<'de> for LocaleValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match SchemaName::deserialize(deserializer)? {
            SchemaName::Text(text) => Ok(Self(Some(text))),
            SchemaName::Locales(_) | SchemaName::Other => Ok(Self(None)),
        }
    }
}

struct SchemaNameVisitor;

impl<'de> Visitor<'de> for SchemaNameVisitor {
    type Value = SchemaName;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a schema name string or locale map")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(non_empty(value).map_or(SchemaName::Other, SchemaName::Text))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut locales = Vec::new();
        while let Some((locale, LocaleValue(name))) = map.next_entry::<String, LocaleValue>()? {
            locales.push((locale, name));
        }
        Ok(SchemaName::Locales(locales))
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(SchemaName::Other)
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(SchemaName::Other)
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(SchemaName::Other)
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(SchemaName::Other)
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(SchemaName::Other)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(SchemaName::Other)
    }
}
