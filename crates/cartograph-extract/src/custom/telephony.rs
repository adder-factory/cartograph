//! Bounded, literal-safe telephony configuration scanners. Includes are graph
//! evidence only; scanners never read another file or evaluate directives.
use cartograph_domain::{FileParseStatus, SourceLanguage, SymbolId, SymbolKind};

use super::{CustomBuilder, CustomSymbolInput, SymbolOptions, specifier_may_carry_credential};
use crate::ExtractError;

mod freeswitch;
mod kamailio;
mod tokens;

const MAXIMUM_NAME_BYTES: usize = 255;

pub(super) fn extract(
    builder: &mut CustomBuilder<'_, '_>,
) -> Result<Option<FileParseStatus>, ExtractError> {
    match builder.snapshot.language() {
        SourceLanguage::Kamailio => kamailio::extract(builder).map(Some),
        SourceLanguage::RtpEngine => rtpengine(builder).map(Some),
        SourceLanguage::Xml => freeswitch::extract(builder),
        _ => Ok(None),
    }
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAXIMUM_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
        && !specifier_may_carry_credential(name)
}

fn symbol(
    builder: &mut CustomBuilder<'_, '_>,
    (kind, name, qualified): (SymbolKind, &str, String),
    (start, end): (usize, usize),
    parent: Option<SymbolId>,
) -> Result<SymbolId, ExtractError> {
    builder.add_symbol(
        CustomSymbolInput::new(kind, name, qualified)
            .at(start, end)
            .with_options(SymbolOptions {
                parent,
                ..SymbolOptions::default()
            }),
    )
}

fn rtpengine(builder: &mut CustomBuilder<'_, '_>) -> Result<FileParseStatus, ExtractError> {
    let mut section: Option<usize> = None;
    let mut complete = true;
    for (offset, line) in super::physical_lines(builder.source()) {
        builder.check_cancelled()?;
        let text = line.trim();
        if text.is_empty() || text.starts_with(['#', ';']) {
            continue;
        }
        if text.starts_with('[') {
            close_section(builder, section.take(), offset)?;
            section = rtpengine_section(builder, (offset, line), text)?;
            complete &= section.is_some();
        } else {
            complete &= rtpengine_option(builder, section, (offset, line), text)?;
        }
    }
    close_section(builder, section, builder.source().len())?;
    Ok(if complete {
        FileParseStatus::Parsed
    } else {
        FileParseStatus::Partial
    })
}

fn close_section(
    builder: &mut CustomBuilder<'_, '_>,
    section: Option<usize>,
    end: usize,
) -> Result<(), ExtractError> {
    if let Some(position) = section {
        let start = usize::try_from(builder.symbols[position].span.start_byte())
            .map_err(|_| ExtractError::InvalidSpan)?;
        builder.symbols[position].span = builder.span(start, end)?;
    }
    Ok(())
}

fn rtpengine_section(
    builder: &mut CustomBuilder<'_, '_>,
    (offset, line): (usize, &str),
    text: &str,
) -> Result<Option<usize>, ExtractError> {
    let Some(end) = text.find(']') else {
        return Ok(None);
    };
    let name = text[1..end].trim();
    let tail = text[end + 1..].trim();
    if !safe_name(name) || !(tail.is_empty() || tail.starts_with(['#', ';'])) {
        return Ok(None);
    }
    let position = builder.symbols.len();
    symbol(
        builder,
        (SymbolKind::Namespace, name, format!("rtpengine::{name}")),
        (offset, offset + line.len()),
        None,
    )?;
    Ok(Some(position))
}

fn rtpengine_option(
    builder: &mut CustomBuilder<'_, '_>,
    section: Option<usize>,
    (offset, line): (usize, &str),
    text: &str,
) -> Result<bool, ExtractError> {
    let Some((key, _)) = text.split_once('=') else {
        return Ok(false);
    };
    let key = key.trim();
    if !safe_name(key) {
        return Ok(false);
    }
    let Some(position) = section else {
        return Ok(false);
    };
    let parent = &builder.symbols[position];
    let qualified = format!("{}::{key}", parent.qualified_name);
    let id = parent.id.clone();
    symbol(
        builder,
        (SymbolKind::Property, key, qualified),
        (offset, offset + line.len()),
        Some(id),
    )?;
    Ok(true)
}
