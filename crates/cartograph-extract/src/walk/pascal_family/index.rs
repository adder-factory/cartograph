//! Per-file structural index for Pascal declarations.
//!
//! Pascal declares a routine once (in a class body, a unit `interface`, or a
//! `forward` declaration) and implements it later in the same file. The index
//! pairs every implementation with its declaration before any fact is emitted,
//! and records each in-file type's members so that implicit `Self` calls and
//! property accessors can name the exact declared member. Pascal identifiers
//! are case-insensitive, so keys are lower-case; resolved names keep the
//! declared spelling. Every retained string is charged to the extraction
//! budget, and a type whose path exceeds the canonical qualified-name bound is
//! left out (its emitted name is shortened, so nothing could bind to it).

use std::collections::{BTreeMap, BTreeSet};

use tree_sitter::Node;

use super::{
    NestedScope,
    names::{IDENTITY_BYTES, name_parts, syntax_identity, type_base_parts},
};
use crate::{
    ExtractError,
    bounded_name::{MAX_CANONICAL_QUALIFIED_NAME_BYTES, shortened_canonical_name},
    walk::{
        ExtractionContext,
        syntax::{has_child_kind, named_children},
    },
};

/// Bound on the recorded inheritance walk; it also stops an in-file cycle.
const MAXIMUM_ANCESTOR_DEPTH: usize = 16;
/// Declarations of one routine name considered when pairing overloads.
const MAXIMUM_OVERLOAD_CANDIDATES: usize = 64;

/// Structural facts about one source file, keyed by lower-case Pascal names.
pub(super) struct FileIndex<'tree> {
    types: BTreeMap<String, TypeEntry>,
    implementation_of: BTreeMap<usize, Node<'tree>>,
    declaration_of: BTreeMap<usize, Node<'tree>>,
    overloaded_routines: BTreeSet<String>,
    /// Declared spelling of each unit-level routine by lower-case name.
    unit_routines: BTreeMap<String, String>,
    used_units: BTreeSet<String>,
}

/// One class, record, object, interface, or helper declared in the file.
struct TypeEntry {
    qualified: String,
    /// The class ancestor, or the extended type of a helper (its `Self`).
    lookup_parent: Option<String>,
    members: BTreeMap<String, Member>,
}

/// A declared member's source spelling and how it may be bound.
struct Member {
    name: String,
    callable: bool,
    declarations: usize,
    strict_private: bool,
}

/// Which members a lookup may bind to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MemberUse {
    /// Calls bind only to routines; a same-named field hides an ancestor routine.
    Call,
    /// Property accessors bind to any field, routine, or property.
    Accessor,
}

/// One member lookup against an in-file type and its in-file ancestors.
#[derive(Clone, Copy)]
pub(super) struct MemberQuery<'query> {
    pub(super) type_key: &'query str,
    pub(super) member: &'query str,
    pub(super) usage: MemberUse,
}

/// The outcome of a member lookup.
pub(super) enum MemberBinding {
    /// No visible member exists along the fully inspected declared ancestry.
    Absent,
    /// The name is a member but not an unambiguous target (an overloaded
    /// routine, or a field where a routine is required).
    Uncertain,
    /// The walk reached a declared ancestor that is not in this file, so an
    /// inherited member of that name cannot be ruled out.
    UnknownAncestry,
    /// The single declared `Type::Member`.
    Exact(String),
}

impl<'tree> FileIndex<'tree> {
    /// Build the index from the root of one parsed file.
    pub(super) fn build(
        root: Node<'tree>,
        maximum_depth: usize,
        context: &mut ExtractionContext<'_, '_>,
    ) -> Result<Self, ExtractError> {
        let source = context.snapshot.source();
        let mut collector = Collector {
            source,
            context,
            maximum_depth,
            types: BTreeMap::new(),
            declarations: BTreeMap::new(),
            implementations: BTreeMap::new(),
            unit_routines: BTreeMap::new(),
            used_units: BTreeSet::new(),
        };
        collector.visit(root, &TypeScope::default())?;
        let Collector {
            types,
            declarations,
            implementations,
            unit_routines,
            used_units,
            ..
        } = collector;
        let Pairing {
            implementation_of,
            declaration_of,
            overloaded_routines,
        } = pair_routines(&declarations, &implementations);
        Ok(Self {
            types,
            implementation_of,
            declaration_of,
            overloaded_routines,
            unit_routines,
            used_units,
        })
    }

    /// The implementation paired with a routine declaration, if any.
    pub(super) fn implementation_of(&self, declaration: Node<'_>) -> Option<Node<'tree>> {
        self.implementation_of
            .get(&declaration.start_byte())
            .copied()
    }

    /// The declaration an implementation was paired with.
    pub(super) fn declaration_of(&self, implementation: Node<'_>) -> Option<Node<'tree>> {
        self.declaration_of
            .get(&implementation.start_byte())
            .copied()
    }

    /// Whether `key` names a type declared (and indexed) in this file.
    pub(super) fn has_type(&self, key: &str) -> bool {
        self.types.contains_key(key)
    }

    /// Whether a lower-case unit-level routine name has several declarations
    /// here, so a call cannot name one without argument matching.
    pub(super) fn is_overloaded_routine(&self, lower: &str) -> bool {
        self.overloaded_routines.contains(lower)
    }

    /// The declared spelling of a unit-level routine (its same-file qualified
    /// name), found by its case-insensitive Pascal name.
    pub(super) fn unit_routine(&self, lower: &str) -> Option<&str> {
        self.unit_routines.get(lower).map(String::as_str)
    }

    /// Whether a `uses` clause in this file names the unit (exact spelling,
    /// as the import binding matches it).
    pub(super) fn uses_unit(&self, unit: &str) -> bool {
        self.used_units.contains(unit)
    }

    /// Bind a member through the in-file ancestry: the type itself, then its
    /// class ancestors (or a helper's extended type). An ancestor's
    /// `strict private` members are invisible to descendants. Absence is
    /// proven only when the walk reaches a type with no declared ancestor; an
    /// ancestry that exceeds the walk bound (or cycles) is uncertain, so it
    /// never reopens ordinary resolution.
    pub(super) fn member_binding(&self, query: MemberQuery<'_>) -> MemberBinding {
        let wanted = query.member.to_ascii_lowercase();
        let mut key = query.type_key.to_owned();
        for level in 0..MAXIMUM_ANCESTOR_DEPTH {
            let Some(entry) = self.types.get(&key) else {
                return MemberBinding::UnknownAncestry;
            };
            if let Some(member) = entry
                .members
                .get(&wanted)
                .filter(|member| level == 0 || !member.strict_private)
            {
                return member.binding(&entry.qualified, query.usage);
            }
            let Some(parent) = entry.lookup_parent.as_deref() else {
                return MemberBinding::Absent;
            };
            let Some(next) = self.parent_key(&key, parent) else {
                return MemberBinding::UnknownAncestry;
            };
            key = next;
        }
        MemberBinding::Uncertain
    }

    /// The indexed key of an ancestor: a sibling nested type first, then a
    /// top-level type.
    fn parent_key(&self, child_key: &str, parent: &str) -> Option<String> {
        if let Some((outer, _)) = child_key.rsplit_once('.') {
            let sibling = format!("{outer}.{parent}");
            if self.types.contains_key(&sibling) {
                return Some(sibling);
            }
        }
        self.types.contains_key(parent).then(|| parent.to_owned())
    }
}

impl Member {
    /// This member as a binding target of `Type::Member`, in the canonical
    /// (possibly shortened) form its emitted symbol is stored under.
    fn binding(&self, qualified: &str, usage: MemberUse) -> MemberBinding {
        let usable = usage == MemberUse::Accessor || self.callable;
        if usable && self.declarations == 1 {
            let target = format!("{qualified}::{}", self.name);
            MemberBinding::Exact(
                shortened_canonical_name(&target, MAX_CANONICAL_QUALIFIED_NAME_BYTES)
                    .unwrap_or(target),
            )
        } else {
            MemberBinding::Uncertain
        }
    }
}

/// Declaration context while collecting: the enclosing type (absent when it
/// was too deep to index), whether the current section is `strict private`,
/// and the nesting depth.
#[derive(Clone, Default)]
struct TypeScope {
    key: Option<String>,
    qualified: Option<String>,
    in_type: bool,
    strict_private: bool,
    depth: usize,
}

impl NestedScope for TypeScope {
    fn depth_mut(&mut self) -> &mut usize {
        &mut self.depth
    }
}

impl TypeScope {
    /// Inside a type that could not be indexed: nothing below is recorded.
    const fn unindexed(&self) -> bool {
        self.in_type && self.key.is_none()
    }
}

/// One routine declaration or implementation and its overload identity.
struct RoutineEntry<'tree> {
    node: Node<'tree>,
    signature: RoutineSignature,
}

/// Overload identity of a routine header: routine kind, parameter modes and
/// type digests (names and default values ignored), and the result type
/// digest. `None` parts were omitted from the header.
#[derive(PartialEq, Eq)]
struct RoutineSignature {
    kind: &'static str,
    parameters: Option<Vec<ParameterGroup>>,
    result: Option<[u8; IDENTITY_BYTES]>,
}

impl RoutineSignature {
    /// An implementation header that omits its parameters and result.
    const fn is_abbreviated(&self) -> bool {
        self.parameters.is_none() && self.result.is_none()
    }

    /// Working memory this identity retains.
    fn retained_bytes(&self) -> u64 {
        let groups = self.parameters.as_ref().map_or(0, Vec::len);
        let bytes =
            size_of::<Self>().saturating_add(groups.saturating_mul(size_of::<ParameterGroup>()));
        u64::try_from(bytes).unwrap_or(u64::MAX)
    }
}

/// Consecutive parameters sharing one mode and type (`A, B: T` and
/// `A: T; B: T` are the same identity).
#[derive(PartialEq, Eq)]
struct ParameterGroup {
    mode: &'static str,
    declared: [u8; IDENTITY_BYTES],
    count: usize,
}

/// Walks the structural (non-body) part of a file and records the index.
struct Collector<'tree, 'context, 'source, 'cancel> {
    source: &'source str,
    context: &'context mut ExtractionContext<'source, 'cancel>,
    maximum_depth: usize,
    types: BTreeMap<String, TypeEntry>,
    declarations: BTreeMap<String, Vec<RoutineEntry<'tree>>>,
    implementations: BTreeMap<String, Vec<RoutineEntry<'tree>>>,
    unit_routines: BTreeMap<String, String>,
    used_units: BTreeSet<String>,
}

impl<'tree> Collector<'tree, '_, '_, '_> {
    /// Charge a retained string to the extraction budget.
    fn charge(&mut self, value: &str) -> Result<(), ExtractError> {
        self.context.budget.reserve_additional_string(value)
    }

    /// Record one structural node, dispatched by kind.
    fn visit(&mut self, node: Node<'tree>, scope: &TypeScope) -> Result<(), ExtractError> {
        self.context.ensure_active()?;
        if scope.depth > self.maximum_depth {
            return Err(ExtractError::NestingLimit);
        }
        match node.kind() {
            "root" | "unit" | "program" | "library" | "interface" | "implementation"
            | "declTypes" | "declVariant" | "declVariantClause" | "ERROR" => {
                self.visit_children(node, &scope.nested())?;
            }
            "declSection" => {
                let section = TypeScope {
                    strict_private: has_child_kind(node, "kStrict")
                        && has_child_kind(node, "kPrivate"),
                    ..scope.nested()
                };
                self.visit_children(node, &section)?;
            }
            "declUses" if !scope.in_type => self.record_uses(node)?,
            "declType" => self.visit_type(node, scope)?,
            "declProc" => self.record_declaration(node, scope)?,
            "defProc" if !scope.in_type => self.record_implementation(node)?,
            "declField" | "declVar" | "declConst" | "declProp" => {
                self.record_members(node, scope)?;
            }
            "declVars" | "declConsts" if scope.in_type => {
                for child in named_children(node) {
                    self.record_members(child, scope)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Record every named child of a container.
    fn visit_children(&mut self, node: Node<'tree>, scope: &TypeScope) -> Result<(), ExtractError> {
        for child in named_children(node) {
            self.visit(child, scope)?;
        }
        Ok(())
    }

    /// Record a class-like type and its members.
    fn visit_type(&mut self, node: Node<'tree>, scope: &TypeScope) -> Result<(), ExtractError> {
        if scope.unindexed() {
            return Ok(());
        }
        let Some(body) = type_body(node) else {
            return Ok(());
        };
        let Some(name) = node
            .child_by_field_name("name")
            .and_then(|name| name_parts(self.source, name))
            .and_then(|parts| parts.last().copied())
        else {
            return Ok(());
        };
        let path = TypePath::nested_in(scope, name);
        let inner = TypeScope {
            key: path.as_ref().map(|path| path.key.clone()),
            qualified: path.as_ref().map(|path| path.qualified.clone()),
            in_type: true,
            strict_private: false,
            depth: scope.depth.saturating_add(1),
        };
        if let Some(path) = path {
            let lookup_parent = lookup_parent_key(self.source, body);
            for value in [
                Some(&path.key),
                Some(&path.qualified),
                lookup_parent.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                self.charge(value)?;
            }
            self.types.entry(path.key).or_insert(TypeEntry {
                qualified: path.qualified,
                lookup_parent,
                members: BTreeMap::new(),
            });
        }
        self.visit_children(body, &inner)
    }

    /// Record the units a `uses` clause names.
    fn record_uses(&mut self, clause: Node<'tree>) -> Result<(), ExtractError> {
        for unit in named_children(clause).filter(|child| child.kind() == "moduleName") {
            if let Some(parts) = name_parts(self.source, unit) {
                let name = parts.join(".");
                self.charge(&name)?;
                self.used_units.insert(name);
            }
        }
        Ok(())
    }

    /// Record a routine declaration and, inside a type, its member.
    fn record_declaration(
        &mut self,
        node: Node<'tree>,
        scope: &TypeScope,
    ) -> Result<(), ExtractError> {
        if scope.unindexed() {
            return Ok(());
        }
        let Some(name) = node
            .child_by_field_name("name")
            .and_then(|name| name_parts(self.source, name))
            .and_then(|parts| parts.last().copied())
        else {
            return Ok(());
        };
        let lower = name.to_ascii_lowercase();
        let key = scope
            .key
            .as_ref()
            .map_or_else(|| lower.clone(), |outer| format!("{outer}.{lower}"));
        if let Some(type_key) = scope.key.clone() {
            self.add_member(
                &type_key,
                Member {
                    name: name.to_owned(),
                    callable: true,
                    declarations: 1,
                    strict_private: scope.strict_private,
                },
            )?;
        } else if !scope.in_type {
            // A declaration names the routine its implementation pairs with.
            self.charge(name)?;
            self.unit_routines.insert(lower.clone(), name.to_owned());
        }
        let signature = self.routine_signature(node)?;
        self.record_routine(RoutineRecord {
            key,
            entry: RoutineEntry { node, signature },
            implementation: false,
        })
    }

    /// Record a unit-level routine implementation.
    fn record_implementation(&mut self, node: Node<'tree>) -> Result<(), ExtractError> {
        let Some(header) = node.child_by_field_name("header") else {
            return Ok(());
        };
        let Some(parts) = header
            .child_by_field_name("name")
            .and_then(|name| name_parts(self.source, name))
        else {
            return Ok(());
        };
        let key = parts.join(".").to_ascii_lowercase();
        if let [name] = parts.as_slice()
            && !self.unit_routines.contains_key(&key)
        {
            self.charge(name)?;
            self.unit_routines.insert(key.clone(), (*name).to_owned());
        }
        let signature = self.routine_signature(header)?;
        self.record_routine(RoutineRecord {
            key,
            entry: RoutineEntry { node, signature },
            implementation: true,
        })
    }

    /// The overload identity of a routine header, charged as it is built.
    fn routine_signature(&mut self, routine: Node<'_>) -> Result<RoutineSignature, ExtractError> {
        let kind = named_children(routine)
            .find_map(|child| match child.kind() {
                "kProcedure" => Some("procedure"),
                "kFunction" => Some("function"),
                "kConstructor" => Some("constructor"),
                "kDestructor" => Some("destructor"),
                "kOperator" => Some("operator"),
                _ => None,
            })
            .unwrap_or_default();
        let parameters = routine
            .child_by_field_name("args")
            .map(|arguments| self.parameter_groups(arguments))
            .transpose()?;
        let result = routine
            .child_by_field_name("type")
            .map(|result| syntax_identity(self.source, result));
        let signature = RoutineSignature {
            kind,
            parameters,
            result,
        };
        self.context
            .budget
            .reserve_working_bytes(signature.retained_bytes())?;
        Ok(signature)
    }

    /// Parameter groups in declaration order, merging adjacent equal groups.
    fn parameter_groups(
        &mut self,
        arguments: Node<'_>,
    ) -> Result<Vec<ParameterGroup>, ExtractError> {
        let mut groups: Vec<ParameterGroup> = Vec::new();
        for argument in named_children(arguments).filter(|child| child.kind() == "declArg") {
            self.context.ensure_active()?;
            let mode = named_children(argument)
                .find_map(|child| match child.kind() {
                    "kConst" => Some("const"),
                    "kVar" => Some("var"),
                    "kOut" => Some("out"),
                    _ => None,
                })
                .unwrap_or_default();
            let declared = argument
                .child_by_field_name("type")
                .map_or([0; IDENTITY_BYTES], |declared| {
                    syntax_identity(self.source, declared)
                });
            let mut cursor = argument.walk();
            let count = argument
                .children_by_field_name("name", &mut cursor)
                .filter(|name| name.kind() == "identifier")
                .count()
                .max(1);
            match groups.last_mut() {
                Some(last) if last.mode == mode && last.declared == declared => {
                    last.count = last.count.saturating_add(count);
                }
                _ => groups.push(ParameterGroup {
                    mode,
                    declared,
                    count,
                }),
            }
        }
        Ok(groups)
    }

    /// File a routine header under its key.
    fn record_routine(&mut self, record: RoutineRecord<'tree>) -> Result<(), ExtractError> {
        self.charge(&record.key)?;
        let routines = if record.implementation {
            &mut self.implementations
        } else {
            &mut self.declarations
        };
        routines.entry(record.key).or_default().push(record.entry);
        Ok(())
    }

    /// Record the non-routine members one declaration names.
    fn record_members(&mut self, node: Node<'tree>, scope: &TypeScope) -> Result<(), ExtractError> {
        let Some(type_key) = scope.key.clone() else {
            return Ok(());
        };
        let mut cursor = node.walk();
        let names = node
            .children_by_field_name("name", &mut cursor)
            .filter(|name| name.kind() == "identifier")
            .filter_map(|name| name_parts(self.source, name))
            .filter_map(|parts| parts.first().copied())
            .collect::<Vec<_>>();
        for name in names {
            self.add_member(
                &type_key,
                Member {
                    name: name.to_owned(),
                    callable: false,
                    declarations: 1,
                    strict_private: scope.strict_private,
                },
            )?;
        }
        Ok(())
    }

    /// Record a member; a repeated name (an overload, or a clash) makes every
    /// binding to it uncertain.
    fn add_member(&mut self, type_key: &str, member: Member) -> Result<(), ExtractError> {
        self.charge(&member.name)?;
        let Some(entry) = self.types.get_mut(type_key) else {
            return Ok(());
        };
        entry
            .members
            .entry(member.name.to_ascii_lowercase())
            .and_modify(|existing| {
                existing.declarations = existing.declarations.saturating_add(1);
                existing.callable &= member.callable;
            })
            .or_insert(member);
        Ok(())
    }
}

/// A routine header to file under its lower-case key.
struct RoutineRecord<'tree> {
    key: String,
    entry: RoutineEntry<'tree>,
    implementation: bool,
}

/// The lower-case dotted lookup key and `Outer::Inner` name of a type.
struct TypePath {
    key: String,
    qualified: String,
}

impl TypePath {
    /// The path of `name` inside `scope`, or `None` past the canonical bound.
    fn nested_in(scope: &TypeScope, name: &str) -> Option<Self> {
        let lower = name.to_ascii_lowercase();
        let (key, qualified) = match (&scope.key, &scope.qualified) {
            (Some(outer_key), Some(outer)) => {
                (format!("{outer_key}.{lower}"), format!("{outer}::{name}"))
            }
            _ => (lower, name.to_owned()),
        };
        (key.len() <= MAX_CANONICAL_QUALIFIED_NAME_BYTES
            && qualified.len() <= MAX_CANONICAL_QUALIFIED_NAME_BYTES)
            .then_some(Self { key, qualified })
    }
}

/// The member-bearing body of a class-like type, or `None` for enums,
/// aliases, and forward declarations.
pub(super) fn type_body(declaration: Node<'_>) -> Option<Node<'_>> {
    named_children(declaration).find(|child| {
        matches!(child.kind(), "declClass" | "declIntf" | "declHelper") && !is_forward(*child)
    })
}

/// `TFoo = class;` and `IFoo = interface;` only announce a later definition.
fn is_forward(body: Node<'_>) -> bool {
    !named_children(body).any(|child| matches!(child.kind(), "kEnd" | "typeref"))
}

/// A class's first declared ancestor, or the type a helper extends (whose
/// members its `Self` exposes).
fn lookup_parent_key(source: &str, body: Node<'_>) -> Option<String> {
    let target = if body.kind() == "declHelper" {
        helper_subject(body)
    } else {
        let mut cursor = body.walk();
        body.children_by_field_name("parent", &mut cursor)
            .find(|parent| parent.kind() == "typeref")
    }?;
    let parts = type_base_parts(source, target)?;
    Some(parts.join(".").to_ascii_lowercase())
}

/// The `TFoo` of `class helper for TFoo`.
fn helper_subject(helper: Node<'_>) -> Option<Node<'_>> {
    let mut after_for = false;
    for child in named_children(helper) {
        if child.kind() == "kFor" {
            after_for = true;
        } else if after_for && child.kind() == "typeref" {
            return Some(child);
        }
    }
    None
}

/// Declaration/implementation pairing and the unit-level names that remain
/// overloaded once every implementation is attached.
struct Pairing<'tree> {
    implementation_of: BTreeMap<usize, Node<'tree>>,
    declaration_of: BTreeMap<usize, Node<'tree>>,
    overloaded_routines: BTreeSet<String>,
}

/// Pair each implementation with the unpaired declaration of the same name
/// and overload identity. An abbreviated header (no parameter list or result
/// type) pairs only with a name that has exactly one declaration. Anything
/// else stays unpaired and emits its own symbol rather than guessing. A
/// unit-level name is overloaded when its declarations plus its unpaired
/// implementations number more than one.
fn pair_routines<'tree>(
    declarations: &BTreeMap<String, Vec<RoutineEntry<'tree>>>,
    implementations: &BTreeMap<String, Vec<RoutineEntry<'tree>>>,
) -> Pairing<'tree> {
    let mut pairing = Pairing {
        implementation_of: BTreeMap::new(),
        declaration_of: BTreeMap::new(),
        overloaded_routines: BTreeSet::new(),
    };
    for (key, bodies) in implementations {
        let candidates = declarations.get(key).map_or(&[] as &[_], Vec::as_slice);
        let paired = pair_key(&mut pairing, candidates, bodies);
        let routines = candidates
            .len()
            .saturating_add(bodies.len().saturating_sub(paired));
        if !key.contains('.') && routines > 1 {
            pairing.overloaded_routines.insert(key.clone());
        }
    }
    for (key, candidates) in declarations {
        if !key.contains('.') && candidates.len() > 1 {
            pairing.overloaded_routines.insert(key.clone());
        }
    }
    pairing
}

/// Pair the implementations of one key; returns how many were paired.
fn pair_key<'tree>(
    pairing: &mut Pairing<'tree>,
    candidates: &[RoutineEntry<'tree>],
    bodies: &[RoutineEntry<'tree>],
) -> usize {
    let mut unpaired = candidates
        .iter()
        .take(MAXIMUM_OVERLOAD_CANDIDATES)
        .collect::<Vec<_>>();
    let mut paired = 0_usize;
    for body in bodies {
        let exact = unpaired
            .iter()
            .position(|candidate| candidate.signature == body.signature);
        let sole =
            (body.signature.is_abbreviated() && candidates.len() == 1 && unpaired.len() == 1)
                .then_some(0);
        let Some(index) = exact.or(sole) else {
            continue;
        };
        let declaration = unpaired.remove(index);
        pairing
            .implementation_of
            .insert(declaration.node.start_byte(), body.node);
        pairing
            .declaration_of
            .insert(body.node.start_byte(), declaration.node);
        paired = paired.saturating_add(1);
    }
    paired
}
