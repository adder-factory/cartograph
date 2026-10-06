//! Delphi VCL (`.dfm`) and FMX (`.fmx`) text form files.
//!
//! A form file streams a component tree: `object Name: TClass` (or
//! `inherited`/`inline`) opens a component, `end` closes it, and
//! `OnEvent = Handler` binds an event to a method. A component streamed with an
//! empty name (`object TLayout`, common in FMX styles) is an anonymous frame:
//! it balances its own `end` but emits no symbol, and handlers inside it belong
//! to the nearest named component. A header this scanner cannot represent (a
//! Unicode component name) is still a frame, so its `end` never closes its
//! parent; it emits nothing and leaves the file partial. Delphi resolves every handler in a form file
//! against the streaming root's class, and a form file belongs to the
//! same-named unit beside it (`{$R *.dfm}` / `{$R *.fmx}` links it). Each
//! handler reference therefore names `RootClass::Handler` and is bound through
//! a named import of that unit file, so it resolves only to the owning unit's
//! declaration and never to a same-named class elsewhere. Property values are
//! never retained; multi-line `( … )` lists, `{ … }` binary data, and `< … >`
//! collections (which nest, and whose items have their own `end` lines) are
//! skipped as opaque values.

use std::collections::BTreeSet;

use cartograph_domain::{FileParseStatus, ReferenceKind, SourceLanguage, SymbolKind};

use super::{
    CustomBuilder, CustomImportInput, CustomSymbolInput, SymbolOptions, basename_stem,
    poll_cancellation,
};
use crate::{
    ExtractError, ExtractedReference, ImportBindingKind, SourceSnapshot,
    budget::reference_budget_bytes, source_lines::physical_lines,
};

/// Form-file extensions owned by the Pascal language mode.
const FORM_EXTENSIONS: [&str; 2] = [".dfm", ".fmx"];
/// Longest component, class, or handler identifier retained.
const MAXIMUM_IDENTIFIER_BYTES: usize = 255;
/// Keywords that open a component frame.
const FRAME_KEYWORDS: [&str; 3] = ["object", "inherited", "inline"];
/// Event-property values that are scalars, not handler names.
const SCALAR_EVENT_VALUES: [&str; 3] = ["nil", "true", "false"];

/// Whether a snapshot is a text form file rather than Pascal source.
pub(super) fn supports(snapshot: &SourceSnapshot) -> bool {
    let path = snapshot.path().as_str();
    snapshot.language() == SourceLanguage::Pascal
        && FORM_EXTENSIONS.iter().any(|extension| {
            path.len() > extension.len()
                && path
                    .get(path.len() - extension.len()..)
                    .is_some_and(|tail| tail.eq_ignore_ascii_case(extension))
        })
}

/// Emit the component tree and event-handler references of one form file.
/// `maximum_nesting` is the configured structural nesting ceiling.
pub(super) fn extract(
    builder: &mut CustomBuilder<'_, '_>,
    maximum_nesting: usize,
) -> Result<FileParseStatus, ExtractError> {
    let form = scan(builder, maximum_nesting)?;
    let positions = emit_components(builder, &form)?;
    emit_handlers(builder, &form, &positions)?;
    Ok(if form.balanced {
        FileParseStatus::Parsed
    } else {
        FileParseStatus::Partial
    })
}

/// Emit each named component under its parent and return the position of
/// each component's symbol in the builder. A child's name is built from its
/// parent's stored (canonically bounded) qualified name, so no full path is
/// retained beside the emitted symbols.
fn emit_components(
    builder: &mut CustomBuilder<'_, '_>,
    form: &FormTree<'_>,
) -> Result<Vec<usize>, ExtractError> {
    let mut positions: Vec<usize> = Vec::with_capacity(form.components.len());
    for component in &form.components {
        let parent = component
            .parent
            .and_then(|index| positions.get(index))
            .and_then(|position| builder.symbols.get(*position));
        let qualified = parent.map_or_else(
            || component.name.to_owned(),
            |outer| format!("{}::{}", outer.qualified_name, component.name),
        );
        let parent = parent.map(|outer| outer.id.clone());
        positions.push(builder.symbols.len());
        builder.add_symbol(
            CustomSymbolInput::new(SymbolKind::Component, component.name, qualified)
                .at(component.start, component.end)
                .with_options(SymbolOptions {
                    signature: Some(component.class.to_owned()),
                    body_search_text: format!("{}: {}", component.name, component.class),
                    parent,
                    ..SymbolOptions::default()
                }),
        )?;
    }
    Ok(positions)
}

/// Emit every handler reference, binding each distinct `RootClass::Handler`
/// once through the owning unit beside the form.
fn emit_handlers(
    builder: &mut CustomBuilder<'_, '_>,
    form: &FormTree<'_>,
    positions: &[usize],
) -> Result<(), ExtractError> {
    let unit = format!("./{}.pas", basename_stem(builder.path()));
    let mut bound = BTreeSet::new();
    for handler in &form.handlers {
        builder.check_cancelled()?;
        let target = format!("{}::{}", handler.root_class, handler.method);
        if !bound.contains(&target) {
            builder.add_import_binding(
                &CustomImportInput::new(None, &unit)
                    .with_kind(ImportBindingKind::Named)
                    .binding(&target, &target)
                    .at(handler.start, handler.end),
            )?;
            builder.budget.reserve_additional_string(&target)?;
            bound.insert(target.clone());
        }
        let owner = positions
            .get(handler.owner)
            .and_then(|position| builder.symbols.get(*position))
            .map(|component| component.id.clone());
        emit_handler(
            builder,
            HandlerReference {
                owner,
                target,
                handler,
            },
        )?;
    }
    Ok(())
}

/// One handler binding with its owning component and `RootClass::Handler`.
struct HandlerReference<'form, 'source> {
    owner: Option<cartograph_domain::SymbolId>,
    target: String,
    handler: &'form Handler<'source>,
}

/// One handler reference owned by its component.
fn emit_handler(
    builder: &mut CustomBuilder<'_, '_>,
    binding: HandlerReference<'_, '_>,
) -> Result<(), ExtractError> {
    let handler = binding.handler;
    let reference = ExtractedReference {
        owner: binding.owner,
        name: handler.method.to_owned(),
        resolution_name: Some(binding.target),
        kind: ReferenceKind::References,
        span: builder.span(handler.start, handler.end)?,
    };
    builder.budget.reserve_fact(
        reference_budget_bytes(&reference),
        [
            reference.name.as_str(),
            reference.resolution_name.as_deref().unwrap_or(""),
        ],
    )?;
    builder.references.push(reference);
    Ok(())
}

/// One named component block.
#[derive(Clone, Copy)]
struct Component<'source> {
    name: &'source str,
    class: &'source str,
    parent: Option<usize>,
    start: usize,
    end: usize,
}

/// One `OnEvent = Handler` binding inside a named component.
struct Handler<'source> {
    owner: usize,
    root_class: &'source str,
    method: &'source str,
    start: usize,
    end: usize,
}

/// The scanned component tree in header order.
struct FormTree<'source> {
    components: Vec<Component<'source>>,
    handlers: Vec<Handler<'source>>,
    balanced: bool,
}

/// One open `object … end` frame: its class (absent for an unsupported
/// header), and the nearest named component at or above it (itself when it
/// is named).
#[derive(Clone, Copy)]
struct OpenFrame<'source> {
    class: Option<&'source str>,
    component: Option<usize>,
    owner: Option<usize>,
}

/// A frame header: a named component, an anonymous frame with only a class,
/// or (with neither) a header whose identifiers cannot be represented.
#[derive(Clone, Copy)]
struct FrameHeader<'source> {
    name: Option<&'source str>,
    class: Option<&'source str>,
    start: usize,
    end: usize,
}

/// A multi-line property value being skipped.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OpaqueValue {
    /// Not inside a multi-line value.
    None,
    /// Inside a value that ends on a line ending with this character.
    Closes(char),
}

/// Line-by-line form state: the tree so far, the open frame stack, any
/// multi-line value being skipped, the depth of nested collections, and how
/// many top-level frames have opened.
struct Scanner<'source> {
    tree: FormTree<'source>,
    open: Vec<OpenFrame<'source>>,
    value: OpaqueValue,
    collections: usize,
    roots: usize,
    maximum_nesting: usize,
}

/// One physical line with its byte offset.
#[derive(Clone, Copy)]
struct FormLine<'source> {
    start: usize,
    raw: &'source str,
}

/// Scan every line into the component tree and handler bindings, charging
/// the retained tree to the extraction's working-memory budget.
fn scan<'source>(
    builder: &mut CustomBuilder<'source, '_>,
    maximum_nesting: usize,
) -> Result<FormTree<'source>, ExtractError> {
    let source = builder.source();
    let mut scanner = Scanner {
        tree: FormTree {
            components: Vec::new(),
            handlers: Vec::new(),
            balanced: true,
        },
        open: Vec::new(),
        value: OpaqueValue::None,
        collections: 0,
        roots: 0,
        maximum_nesting,
    };
    let mut next_poll = 0;
    for (start, raw) in physical_lines(source) {
        poll_cancellation(builder.cancelled, start, &mut next_poll)?;
        if let Some(retained) = scanner.line(FormLine { start, raw })? {
            builder.budget.reserve_working_bytes(retained)?;
        }
    }
    let complete =
        scanner.open.is_empty() && scanner.value == OpaqueValue::None && scanner.collections == 0;
    scanner.tree.balanced &= complete;
    Ok(scanner.tree)
}

impl<'source> Scanner<'source> {
    /// Advance the scanner by one physical line, returning the working bytes
    /// a newly retained component or handler occupies.
    fn line(&mut self, line: FormLine<'source>) -> Result<Option<u64>, ExtractError> {
        let text = line.raw.trim();
        if let OpaqueValue::Closes(closing) = self.value {
            if text.ends_with(closing) {
                self.value = OpaqueValue::None;
            }
            return Ok(None);
        }
        if let Some(closing) = opaque_value_opener(text) {
            self.value = OpaqueValue::Closes(closing);
            return Ok(None);
        }
        if self.collections > 0 {
            self.collection_line(text);
            return Ok(None);
        }
        if collection_value(text) == Some(CollectionValue::Opens) {
            self.collections = 1;
            return Ok(None);
        }
        if text.eq_ignore_ascii_case("end") {
            self.close(line);
            return Ok(None);
        }
        if let Some(header) = frame_header(line) {
            return self.open(header);
        }
        Ok(event_binding(line).and_then(|handler| self.bind(handler)))
    }

    /// Track nested `< … >` collections while inside one: a collection value
    /// that opens here nests one deeper, one that also closes on this line
    /// leaves the depth unchanged, and any other line ending in `>` closes
    /// the innermost collection (`end>`).
    fn collection_line(&mut self, text: &str) {
        match collection_value(text) {
            Some(CollectionValue::Opens) => {
                self.collections = self.collections.saturating_add(1);
            }
            None if text.ends_with('>') => {
                self.collections = self.collections.saturating_sub(1);
            }
            Some(CollectionValue::Complete) | None => {}
        }
    }

    /// Open a frame under the innermost open one; a named frame is a component.
    fn open(&mut self, header: FrameHeader<'source>) -> Result<Option<u64>, ExtractError> {
        if self.open.len() >= self.maximum_nesting {
            return Err(ExtractError::NestingLimit);
        }
        if self.open.is_empty() {
            if self.roots > 0 {
                self.tree.balanced = false;
            }
            self.roots = self.roots.saturating_add(1);
        }
        if header.class.is_none() {
            self.tree.balanced = false;
        }
        let enclosing = self.open.last().and_then(|frame| frame.owner);
        let component = header.name.zip(header.class).map(|(name, class)| {
            self.tree.components.push(Component {
                name,
                class,
                parent: enclosing,
                start: header.start,
                end: header.end,
            });
            self.tree.components.len().saturating_sub(1)
        });
        self.open.push(OpenFrame {
            class: header.class,
            component,
            owner: component.or(enclosing),
        });
        Ok(component.map(|_| retained_bytes::<Component<'_>>()))
    }

    /// Close the innermost frame at an `end` line.
    fn close(&mut self, line: FormLine<'_>) {
        let Some(frame) = self.open.pop() else {
            self.tree.balanced = false;
            return;
        };
        if let Some(component) = frame
            .component
            .and_then(|index| self.tree.components.get_mut(index))
        {
            component.end = line.start.saturating_add(line.raw.trim_end().len());
        }
    }

    /// Attach a handler binding to the nearest open named component; the
    /// outermost frame's class is the streaming root every handler names.
    fn bind(&mut self, handler: (usize, usize, &'source str)) -> Option<u64> {
        let owner = self.open.last()?.owner?;
        let root_class = self.open.first()?.class?;
        let (start, end, method) = handler;
        self.tree.handlers.push(Handler {
            owner,
            root_class,
            method,
            start,
            end,
        });
        Some(retained_bytes::<Handler<'_>>())
    }
}

/// Working bytes one retained scanner record occupies.
fn retained_bytes<Record>() -> u64 {
    u64::try_from(size_of::<Record>()).unwrap_or(u64::MAX)
}

/// `Name = (…` and `Data = {…` start values that end on a later line: the
/// value opens a list or binary block and does not close on the same line.
fn opaque_value_opener(text: &str) -> Option<char> {
    let (_, value) = text.split_once('=')?;
    let value = value.trim();
    let closing = match value.chars().next()? {
        '(' => ')',
        '{' => '}',
        _ => return None,
    };
    (!value.ends_with(closing)).then_some(closing)
}

/// A `Name = < … ` collection value on one line.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CollectionValue {
    /// The collection (possibly with its first item) continues on later lines.
    Opens,
    /// The whole collection (`<>`, `<item end>`) is on this line.
    Complete,
}

/// Whether a property line's value is a collection, and whether it closes on
/// the same line.
fn collection_value(text: &str) -> Option<CollectionValue> {
    let (_, value) = text.split_once('=')?;
    let value = value.trim();
    if !value.starts_with('<') {
        return None;
    }
    Some(if value.ends_with('>') {
        CollectionValue::Complete
    } else {
        CollectionValue::Opens
    })
}

/// `object Name: TClass`, `inherited Name: TClass [0]`, `inline Name: TClass`,
/// or an unnamed `object TClass` frame. A frame keyword alone or followed by
/// anything but an assignment always opens a frame, even when its
/// identifiers cannot be represented, so the `end` that closes it stays
/// balanced.
fn frame_header(line: FormLine<'_>) -> Option<FrameHeader<'_>> {
    let trimmed = line.raw.trim_start();
    let indent = line.raw.len() - trimmed.len();
    let (keyword, rest) = trimmed
        .split_once(char::is_whitespace)
        .unwrap_or((trimmed.trim_end(), ""));
    if rest.contains('=')
        || !FRAME_KEYWORDS
            .iter()
            .any(|expected| expected.eq_ignore_ascii_case(keyword))
    {
        return None;
    }
    let (name, class) =
        header_identifiers(rest).map_or((None, None), |(name, class)| (name, Some(class)));
    Some(FrameHeader {
        name,
        class,
        start: line.start.saturating_add(indent),
        end: line.start.saturating_add(line.raw.trim_end().len()),
    })
}

/// The optional component name and the class of a frame header's text
/// after its keyword, or `None` when either is not a plain identifier.
fn header_identifiers(rest: &str) -> Option<(Option<&str>, &str)> {
    let (name, class) = match rest.split_once(':') {
        Some((name, class)) => (Some(identifier(name.trim())?), class),
        None => (None, rest),
    };
    let mut tokens = class.split_whitespace();
    let class = identifier(tokens.next()?)?;
    tokens
        .next()
        .is_none_or(|index| index.starts_with('['))
        .then_some((name, class))
}

/// `OnEvent = Handler` with plain identifiers on both sides. The event name
/// follows Delphi's `On` + capitalized-word convention, so ordinary
/// properties such as `OnlyDigits = True` are not handlers, and a scalar
/// value (`nil`, `True`, `False`) never names a method.
fn event_binding(line: FormLine<'_>) -> Option<(usize, usize, &str)> {
    let (event, handler) = line.raw.split_once('=')?;
    let event = event.trim();
    let is_event = event
        .strip_prefix("On")
        .and_then(|rest| rest.bytes().next())
        .is_some_and(|first| first.is_ascii_uppercase());
    if !is_event || identifier(event).is_none() {
        return None;
    }
    let method = identifier(handler.trim()).filter(|method| {
        !SCALAR_EVENT_VALUES
            .iter()
            .any(|scalar| scalar.eq_ignore_ascii_case(method))
    })?;
    let offset = line.raw.len() - handler.trim_start().len();
    let start = line.start.saturating_add(offset);
    Some((start, start.saturating_add(method.len()), method))
}

/// A plain ASCII Pascal identifier within the retained-length bound.
fn identifier(text: &str) -> Option<&str> {
    let first = text.bytes().next()?;
    (text.len() <= MAXIMUM_IDENTIFIER_BYTES
        && !first.is_ascii_digit()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'))
    .then_some(text)
}
