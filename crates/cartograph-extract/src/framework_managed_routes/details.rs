//! Declaration-indexed managed routes and annotation landmarks.

use std::collections::BTreeMap;

use crate::framework::Quoted;

use super::{
    AnnotationArgument, ContextSlice, DeclarationContext, ExtractError, FrameworkBuilder,
    FrameworkRouteInput, Mapping, SymbolId, SymbolKind, annotation_argument,
    annotation_argument_in_slice, declaration_context, identifier_byte, original_callable,
    original_declaration, skip_ascii_whitespace,
};

const MEMBER_INDEX_ENTRY_BYTES: u64 = 192;
const MAX_HTTP_MAPPINGS: usize = 32;
const HTTP_ANNOTATIONS: &[(&str, &str)] = &[
    ("[HttpGet", "GET"),
    ("[HttpPost", "POST"),
    ("[HttpPut", "PUT"),
    ("[HttpPatch", "PATCH"),
    ("[HttpDelete", "DELETE"),
    ("[HttpHead", "HEAD"),
    ("[HttpOptions", "OPTIONS"),
];

pub(super) fn literal_operand(argument: &str, quoted: &Quoted<'_>) -> bool {
    let prefix = argument[..quoted.start - 1].trim();
    let positional = prefix.is_empty();
    let named = prefix
        .strip_suffix(':')
        .is_some_and(|name| name.trim() == "template");
    let suffix = argument[quoted.end + 1..].trim_start();
    (positional || named) && (suffix.is_empty() || suffix.starts_with(','))
}

pub(super) fn landmarks(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    spring: bool,
) -> Result<(), ExtractError> {
    for index in 0..builder.original_symbol_count() {
        builder.bridge.charge_work(1)?;
        let declaration = original_declaration(builder, index, SymbolKind::Class).or_else(|| {
            (!spring)
                .then(|| original_callable(builder, index))
                .flatten()
        });
        let Some(declaration) = declaration else {
            continue;
        };
        let context = declaration_context(source, declaration.start, declaration.end);
        if spring {
            spring_base(builder, context)?;
        } else {
            aspnet_landmarks(builder, context)?;
        }
    }
    Ok(())
}

pub(super) struct Members(BTreeMap<SymbolId, Vec<usize>>);

impl Members {
    pub(super) fn build(builder: &mut FrameworkBuilder<'_, '_>) -> Result<Self, ExtractError> {
        let count = builder.original_symbol_count();
        builder.bridge.reserve_working_bytes(
            MEMBER_INDEX_ENTRY_BYTES
                .saturating_mul(u64::try_from(count).map_err(|_| ExtractError::OutputLimit)?),
        )?;
        let mut methods = BTreeMap::new();
        for index in 0..count {
            builder.bridge.charge_work(1)?;
            if let Some(method) = original_callable(builder, index) {
                methods.insert(method.id, index);
            }
        }
        let mut members: BTreeMap<SymbolId, Vec<usize>> = BTreeMap::new();
        for index in 0..builder.containments().len() {
            builder.bridge.charge_work(1)?;
            let containment = &builder.containments()[index];
            if let Some(&method) = methods.get(&containment.child) {
                members
                    .entry(containment.parent.clone())
                    .or_default()
                    .push(method);
            }
        }
        Ok(Self(members))
    }

    pub(super) fn of(&self, class: &SymbolId) -> &[usize] {
        self.0.get(class).map_or(&[], Vec::as_slice)
    }
}

fn spring_base(
    builder: &mut FrameworkBuilder<'_, '_>,
    context: DeclarationContext<'_>,
) -> Result<(), ExtractError> {
    let Some(AnnotationArgument::Literal { value, start, end }) =
        annotation_argument(context, "@RequestMapping")
    else {
        return Ok(());
    };
    // Only a single positional literal is a BASE landmark; named/array mappings
    // retain their existing endpoint behavior.
    if !positional_base(context) {
        return Ok(());
    }
    builder.add_route(FrameworkRouteInput {
        method: "BASE",
        path: value,
        start,
        end,
        command: false,
        handler: None,
    })
}

fn positional_base(context: DeclarationContext<'_>) -> bool {
    let Some(slice) = context
        .into_iter()
        .find(|slice| slice.text.contains("@RequestMapping"))
    else {
        return false;
    };
    let Some(marker) = slice.text.find("@RequestMapping") else {
        return false;
    };
    let open = skip_ascii_whitespace(slice.text, marker + "@RequestMapping".len());
    slice.text.as_bytes().get(open) == Some(&b'(')
        && matches!(
            slice
                .text
                .as_bytes()
                .get(skip_ascii_whitespace(slice.text, open + 1)),
            Some(b'\'' | b'"')
        )
}

fn aspnet_landmarks(
    builder: &mut FrameworkBuilder<'_, '_>,
    context: DeclarationContext<'_>,
) -> Result<(), ExtractError> {
    for argument in arguments(context, "[Route") {
        if let AnnotationArgument::Literal { value, start, end } = argument {
            builder.add_route(FrameworkRouteInput {
                method: "ROUTE",
                path: value,
                start,
                end,
                command: false,
                handler: None,
            })?;
        }
    }
    Ok(())
}

pub(super) fn aspnet_mappings(context: DeclarationContext<'_>) -> Vec<Mapping<'_>> {
    let mut mappings = Vec::new();
    for &(annotation, method) in HTTP_ANNOTATIONS {
        for argument in arguments(context, annotation) {
            mappings.push(Mapping { method, argument });
        }
    }
    mappings
}

fn arguments<'s>(context: DeclarationContext<'s>, marker: &str) -> Vec<AnnotationArgument<'s>> {
    let mut arguments = Vec::new();
    for mut slice in context {
        while arguments.len() < MAX_HTTP_MAPPINGS {
            let Some(start) = slice.text.find(marker) else {
                break;
            };
            let at = ContextSlice {
                text: &slice.text[start..],
                offset: slice.offset + start,
            };
            if !at
                .text
                .as_bytes()
                .get(marker.len())
                .is_some_and(|byte| identifier_byte(*byte))
                && let Some(argument) = annotation_argument_in_slice(at, marker)
            {
                arguments.push(argument);
            }
            let next = start + marker.len();
            slice = ContextSlice {
                text: &slice.text[next..],
                offset: slice.offset + next,
            };
        }
    }
    arguments
}
