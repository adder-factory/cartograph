use cartograph_domain::{FileParseStatus, ReferenceKind, SymbolKind};

use super::super::{CustomImportInput, CustomReferenceInput, poll_cancellation};
use super::tokens::{self, Kind, Token};
use super::{CustomBuilder, ExtractError, safe_name, symbol};
use crate::ImportBindingKind;

const ROUTES: &[&str] = &[
    "route",
    "request_route",
    "reply_route",
    "branch_route",
    "failure_route",
    "onreply_route",
    "onsend_route",
    "event_route",
    "branch_failure_route",
];

pub(super) fn extract(
    builder: &mut CustomBuilder<'_, '_>,
) -> Result<FileParseStatus, ExtractError> {
    let (tokens, mut complete) = tokens::scan(builder)?;
    let mut state = RoutingState::default();
    for index in 0..tokens.len() {
        builder.check_cancelled()?;
        if !state.read(builder, &tokens, index)? {
            complete = false;
            break;
        }
    }
    complete &= state.delimiters.is_empty() && state.pending.is_none();
    Ok(if complete {
        FileParseStatus::Parsed
    } else {
        FileParseStatus::Partial
    })
}

#[derive(Default)]
struct RoutingState {
    delimiters: Vec<(u8, Option<usize>)>,
    pending: Option<(usize, usize)>,
    opaque_until: usize,
}

impl RoutingState {
    fn read(
        &mut self,
        builder: &mut CustomBuilder<'_, '_>,
        tokens: &[Token],
        index: usize,
    ) -> Result<bool, ExtractError> {
        let token = tokens[index];
        if token.start < self.opaque_until {
            return Ok(true);
        }
        if token.kind == Kind::Directive {
            self.opaque_until = directive_end(builder, token.start)?;
            let count = tokens[index..].partition_point(|token| token.start < self.opaque_until);
            include(builder, &tokens[index..index + count])?;
            top_level(builder, &tokens[index..index + count])?;
            return Ok(true);
        }
        include(builder, &tokens[index..])?;
        if self.delimiters.is_empty() {
            if let Some(pending) = declare_route(builder, tokens, index)? {
                self.pending = Some(pending);
            }
            top_level(builder, &tokens[index..])?;
        } else if is_call(builder.source(), tokens, index) {
            let owner = self
                .delimiters
                .iter()
                .rev()
                .find_map(|(_, position)| *position);
            call(builder, &tokens[index..], owner)?;
        }
        self.delimiter(builder, token, index)
    }

    fn delimiter(
        &mut self,
        builder: &mut CustomBuilder<'_, '_>,
        token: Token,
        index: usize,
    ) -> Result<bool, ExtractError> {
        match token.kind {
            Kind::Punctuation(open @ (b'{' | b'(' | b'[')) => {
                if self.delimiters.len() >= builder.maximum_ast_depth {
                    return Err(ExtractError::NestingLimit);
                }
                builder.budget.reserve_working_bytes(
                    u64::try_from(2 * std::mem::size_of::<(u8, Option<usize>)>())
                        .map_err(|_| ExtractError::OutputLimit)?,
                )?;
                let position = self
                    .pending
                    .filter(|(opening, _)| *opening == index)
                    .map(|(_, position)| position);
                if position.is_some() {
                    self.pending = None;
                }
                self.delimiters.push((open, position));
            }
            Kind::Punctuation(close @ (b'}' | b')' | b']')) => {
                return self.close(builder, token, close);
            }
            _ => {}
        }
        Ok(true)
    }

    fn close(
        &mut self,
        builder: &mut CustomBuilder<'_, '_>,
        token: Token,
        close: u8,
    ) -> Result<bool, ExtractError> {
        let Some((open, position)) = self.delimiters.pop() else {
            return Ok(false);
        };
        if !matches!((open, close), (b'{', b'}') | (b'(', b')') | (b'[', b']')) {
            return Ok(false);
        }
        if let Some(position) = position {
            let start = usize::try_from(builder.symbols[position].span.start_byte())
                .map_err(|_| ExtractError::InvalidSpan)?;
            builder.symbols[position].span = builder.span(start, token.end)?;
        }
        Ok(true)
    }
}

fn declare_route(
    builder: &mut CustomBuilder<'_, '_>,
    tokens: &[Token],
    index: usize,
) -> Result<Option<(usize, usize)>, ExtractError> {
    let source = builder.source();
    let Some((label, opening)) = route_header(source, &tokens[index..]) else {
        return Ok(None);
    };
    let position = builder.symbols.len();
    let token = tokens[index];
    let kind = token.text(source);
    let qualified = if opening == 1 {
        format!("kamailio::{kind}")
    } else {
        route_name(kind, label)
    };
    symbol(
        builder,
        (SymbolKind::Function, label, qualified),
        (token.start, tokens[opening + index].end),
        None,
    )?;
    Ok(Some((opening + index, position)))
}

fn is_call(source: &str, tokens: &[Token], index: usize) -> bool {
    tokens[index].kind == Kind::Word
        && tokens
            .get(index + 1)
            .is_some_and(|next| next.kind == Kind::Punctuation(b'('))
        && !matches!(
            tokens[index].text(source),
            "if" | "while" | "switch" | "for"
        )
        && !index
            .checked_sub(1)
            .and_then(|previous| tokens.get(previous))
            .is_some_and(|previous| previous.kind == Kind::Punctuation(b'$'))
}

/// A directive's continued physical lines are all opaque until expansion.
fn directive_end(builder: &mut CustomBuilder<'_, '_>, start: usize) -> Result<usize, ExtractError> {
    let bytes = builder.source().as_bytes();
    let mut next_poll = start;
    for cursor in start..bytes.len() {
        poll_cancellation(builder.cancelled, cursor, &mut next_poll)?;
        if bytes[cursor] == b'\n' {
            let previous = if cursor > start && bytes[cursor - 1] == b'\r' {
                cursor - 1
            } else {
                cursor
            };
            if previous == start || bytes[previous - 1] != b'\\' {
                return Ok(cursor);
            }
        }
    }
    Ok(bytes.len())
}

fn route_name(kind: &str, label: &str) -> String {
    format!("kamailio::{kind}::{label}")
}

fn route_header<'source>(source: &'source str, tokens: &[Token]) -> Option<(&'source str, usize)> {
    let first = *tokens.first()?;
    if first.kind != Kind::Word || !ROUTES.contains(&first.text(source)) {
        return None;
    }
    match tokens.get(1)?.kind {
        Kind::Punctuation(b'{') => Some((first.text(source), 1)),
        Kind::Punctuation(b'[')
            if tokens.get(3)?.kind == Kind::Punctuation(b']')
                && tokens.get(4)?.kind == Kind::Punctuation(b'{') =>
        {
            let name = tokens
                .get(2)?
                .value(source)
                .filter(|name| safe_name(name))?;
            Some((name, 4))
        }
        _ => None,
    }
}

fn include(builder: &mut CustomBuilder<'_, '_>, tokens: &[Token]) -> Result<(), ExtractError> {
    let source = builder.source();
    let text = tokens[0].keyword(source);
    if matches!(text, "include_file" | "import_file" | "include")
        && let Some(argument) = tokens
            .get(1)
            .copied()
            .filter(|token| token.kind == Kind::String)
        && let Some(path) = argument
            .value(source)
            .filter(|path| !path.is_empty() && !path.contains('$'))
    {
        builder.add_import(
            &CustomImportInput::new(None, path)
                .with_kind(ImportBindingKind::IncludeQuoted)
                .at(argument.start + 1, argument.end - 1),
        )?;
    }
    Ok(())
}

fn top_level(builder: &mut CustomBuilder<'_, '_>, tokens: &[Token]) -> Result<(), ExtractError> {
    let source = builder.source();
    let first = tokens[0];
    let text = first.keyword(source);
    if first.kind == Kind::Directive
        && matches!(text, "define" | "trydef" | "redefine")
        && let Some(name) = tokens
            .get(1)
            .copied()
            .filter(|token| token.kind == Kind::Word && safe_name(token.text(source)))
    {
        let name_text = name.text(source);
        symbol(
            builder,
            (
                SymbolKind::Constant,
                name_text,
                format!("kamailio::define::{name_text}"),
            ),
            (name.start, name.end),
            None,
        )?;
    }
    if text == "loadmodule"
        && let Some(token) = tokens
            .get(1)
            .copied()
            .filter(|token| token.kind == Kind::String)
        && let Some(module) = token.value(source).filter(|name| safe_name(name))
    {
        builder.add_reference(
            CustomReferenceInput::new(None, module, ReferenceKind::Imports)
                .at(token.start + 1, token.end - 1),
        )?;
    }
    if text == "modparam"
        && tokens
            .get(1)
            .is_some_and(|token| token.kind == Kind::Punctuation(b'('))
        && tokens
            .get(3)
            .is_some_and(|token| token.kind == Kind::Punctuation(b','))
        && let Some(module) = tokens
            .get(2)
            .and_then(|token| token.value(source))
            .filter(|name| safe_name(name))
        && let Some(parameter) = tokens.get(4).copied()
        && let Some(name) = parameter.value(source).filter(|name| safe_name(name))
    {
        symbol(
            builder,
            (
                SymbolKind::Property,
                name,
                format!("kamailio::module::{module}::{name}"),
            ),
            (parameter.start, parameter.end),
            None,
        )?;
    }
    if first.kind == Kind::Word
        && safe_name(text)
        && tokens
            .get(1)
            .is_some_and(|token| token.kind == Kind::Punctuation(b'='))
    {
        symbol(
            builder,
            (
                SymbolKind::Property,
                text,
                format!("kamailio::core::{text}"),
            ),
            (first.start, first.end),
            None,
        )?;
    }
    Ok(())
}

fn call(
    builder: &mut CustomBuilder<'_, '_>,
    tokens: &[Token],
    owner: Option<usize>,
) -> Result<(), ExtractError> {
    let token = tokens[0];
    let source = builder.source();
    let name = token.text(source);
    if !safe_name(name) {
        return Ok(());
    }
    let owner = owner.map(|position| builder.symbols[position].id.clone());
    let target_kind = match name {
        "route" | "route_if_exists" | "async_route" => Some(("route", ReferenceKind::Calls)),
        "t_on_failure" => Some(("failure_route", ReferenceKind::References)),
        "t_on_reply" => Some(("onreply_route", ReferenceKind::References)),
        "t_on_branch" => Some(("branch_route", ReferenceKind::References)),
        _ => None,
    };
    if let Some((route, kind)) = target_kind
        && let Some(argument) = tokens.get(2).copied()
        && let Some(label) = argument.value(source).filter(|name| safe_name(name))
        && tokens
            .get(3)
            .is_some_and(|token| matches!(token.kind, Kind::Punctuation(b')' | b',')))
    {
        let adjustment = usize::from(argument.kind == Kind::String);
        return builder.add_reference(
            CustomReferenceInput::new(owner, label, kind)
                .with_resolution(&route_name(route, label))
                .at(argument.start + adjustment, argument.end - adjustment),
        );
    }
    builder.add_reference(
        CustomReferenceInput::new(owner, name, ReferenceKind::Calls)
            .with_resolution(&format!("kamailio::builtin::{name}"))
            .at(token.start, token.end),
    )
}
