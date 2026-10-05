//! PHP compile-time name resolution.
//!
//! PHP resolves class, interface, trait, enum, and qualified function names
//! syntactically: a fully qualified name is used as written, `namespace\X` is
//! relative to the current namespace, a name whose first segment is a `use`
//! alias expands through that alias, and any other name is relative to the
//! current namespace. Unqualified function names consult the function import
//! table and otherwise try the current namespace and then the global function.
//! Aliases are scoped to their namespace block and match case-insensitively,
//! like PHP class and function names.

use std::collections::BTreeMap;

use crate::PHP_EXACT_RESOLUTION_PREFIX;

/// Longest PHP name, include path, or rendered receiver retained in a fact.
pub(super) const MAX_NAME_BYTES: usize = 512;
/// Longest synthesized lookup (`marker` + intent + `Namespace::Class::member`).
const MAX_LOOKUP_BYTES: usize = 1_536;
/// Upper bound on recorded `use` aliases per namespace block; past it, names
/// that could depend on an unrecorded alias are left unresolved.
const MAX_ALIASES: usize = 4_096;
/// Separator between candidate-key segments, as produced by qualified names.
pub(super) const KEY_SEPARATOR: &str = "::";
/// Parts every exact lookup starts with: marker, intent, and separator.
const LOOKUP_HEADER_PARTS: usize = 3;

/// The PHP symbol space a `use` clause imports into.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ImportKind {
    Class,
    Function,
    Constant,
}

impl ImportKind {
    /// The lookup intent of a reference to the imported declaration itself.
    pub(super) const fn intent(self) -> Intent {
        match self {
            Self::Class => Intent::Class,
            Self::Function => Intent::Function,
            Self::Constant => Intent::Constant,
        }
    }
}

/// Which declarations an exact PHP lookup may bind to.
///
/// The dispatch intents name the declaration in the statically known class
/// for `static::` and `$this->`, which late static binding or an override can
/// replace at runtime; `AdaptedClass` names a trait used with an adaptation
/// block, exactly like `Class` but marking that the block can add or rename
/// the methods it supplies; `ReturnedMember` names a factory method and the member
/// called on the object it returns, which the indexer follows only through the
/// factory's declared return type, and `OwnReturnedMember` does so for a call
/// lexically inside the factory's own class, where a non-public factory is
/// accessible; `Abstain` marks a static name whose target cannot be
/// determined exactly and must not be searched for by short name.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Intent {
    Class,
    AdaptedClass,
    DispatchClass,
    Function,
    FunctionFallback,
    Member,
    DispatchMember,
    ReturnedMember,
    OwnReturnedMember,
    Constant,
    Abstain,
}

impl Intent {
    const fn marker(self) -> &'static str {
        match self {
            Self::Class => "class",
            Self::AdaptedClass => "adapted-class",
            Self::DispatchClass => "dispatch-class",
            Self::Function => "function",
            Self::FunctionFallback => "function-fallback",
            Self::Member => "member",
            Self::DispatchMember => "dispatch-member",
            Self::ReturnedMember => "returned-member",
            Self::OwnReturnedMember => "own-returned-member",
            Self::Constant => "constant",
            Self::Abstain => "abstain",
        }
    }

    const fn scoped(member: bool, dispatch: bool) -> Self {
        match (member, dispatch) {
            (true, true) => Self::DispatchMember,
            (true, false) => Self::Member,
            (false, true) => Self::DispatchClass,
            (false, false) => Self::Class,
        }
    }
}

/// Lexical class context for `self`, `static`, `parent`, and `$this`.
pub(super) struct ClassContext {
    /// Exact qualified name of the enclosing class-like declaration.
    pub(super) key: Option<String>,
    /// Candidate key of the class named by its `extends` clause.
    pub(super) parent: Option<String>,
    /// A trait body: `self` there denotes the class that uses the trait, so
    /// its members are late-bound and the trait itself is never the class.
    pub(super) trait_body: bool,
}

impl ClassContext {
    /// The scope of a named function body, which PHP compiles without a
    /// class even when the function is declared inside a method.
    pub(super) const OUTSIDE: Self = Self {
        key: None,
        parent: None,
        trait_body: false,
    };
}

/// Per-file PHP name-resolution state threaded through the walker.
#[derive(Default)]
pub(in crate::walk) struct PhpScope {
    namespace: Option<String>,
    unbraced_namespace: bool,
    classes: BTreeMap<String, String>,
    functions: BTreeMap<String, String>,
    aliases_overflowed: bool,
    contexts: Vec<ClassContext>,
}

impl PhpScope {
    /// Start a namespace block; `use` aliases never cross block boundaries.
    pub(super) fn enter_namespace(&mut self, namespace: Option<String>, unbraced: bool) {
        self.namespace = namespace;
        self.unbraced_namespace = unbraced;
        self.classes.clear();
        self.functions.clear();
        self.aliases_overflowed = false;
    }

    pub(super) fn namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    pub(super) const fn unbraced_namespace(&self) -> bool {
        self.unbraced_namespace
    }

    /// Record that this block has an import the tables cannot represent, so
    /// names that could depend on it abstain instead of resolving relative to
    /// the namespace.
    pub(super) fn mark_aliases_incomplete(&mut self) {
        self.aliases_overflowed = true;
    }

    /// Record one `use` alias of the current namespace block.
    pub(super) fn register_alias(&mut self, kind: ImportKind, local: &str, fqn: &str) {
        let table = match kind {
            ImportKind::Class => &self.classes,
            ImportKind::Function => &self.functions,
            ImportKind::Constant => return,
        };
        let folded = local.to_ascii_lowercase();
        if table.contains_key(&folded) {
            return;
        }
        if self.classes.len().saturating_add(self.functions.len()) >= MAX_ALIASES {
            self.aliases_overflowed = true;
            return;
        }
        let Some(target) = bounded_concat(&[fqn], MAX_NAME_BYTES) else {
            self.aliases_overflowed = true;
            return;
        };
        let table = if kind == ImportKind::Class {
            &mut self.classes
        } else {
            &mut self.functions
        };
        table.insert(folded, target);
    }

    pub(super) fn push_class(&mut self, context: ClassContext) {
        self.contexts.push(context);
    }

    pub(super) fn pop_class(&mut self) {
        self.contexts.pop();
    }

    fn current_class(&self) -> Option<&ClassContext> {
        self.contexts.last()
    }

    fn namespaced(&self, relative: &str) -> Option<String> {
        match self.namespace.as_deref() {
            Some(namespace) => bounded_concat(&[namespace, "\\", relative], MAX_NAME_BYTES),
            None => bounded_concat(&[relative], MAX_NAME_BYTES),
        }
    }

    /// The fully qualified name a class-like name denotes, or `None` when it
    /// cannot be determined exactly.
    pub(super) fn class_fqn(&self, raw: &str) -> Option<String> {
        if let Some(fully_qualified) = raw.strip_prefix('\\') {
            return valid_fqn(fully_qualified)
                .then(|| bounded_concat(&[fully_qualified], MAX_NAME_BYTES))
                .flatten();
        }
        if self.aliases_overflowed {
            return None;
        }
        if let Some(relative) = strip_ascii_prefix(raw, "namespace\\") {
            return self.namespaced(relative);
        }
        let (head, tail) = raw
            .split_once('\\')
            .map_or((raw, None), |(head, tail)| (head, Some(tail)));
        match (self.classes.get(&head.to_ascii_lowercase()), tail) {
            (Some(alias), Some(tail)) => bounded_concat(&[alias, "\\", tail], MAX_NAME_BYTES),
            (Some(alias), None) => bounded_concat(&[alias], MAX_NAME_BYTES),
            (None, _) => self.namespaced(raw),
        }
    }

    /// The candidate key of a class-like name and whether it is late-bound,
    /// honoring `self`, `static`, and `parent` in the current class context.
    fn class_key(&self, raw: &str) -> (Option<String>, bool) {
        let context = self.current_class();
        let trait_body = context.is_some_and(|class| class.trait_body);
        match raw.to_ascii_lowercase().as_str() {
            "self" => (context.and_then(|class| class.key.clone()), trait_body),
            "static" => (context.and_then(|class| class.key.clone()), true),
            "parent" => (context.and_then(|class| class.parent.clone()), false),
            _ => (self.class_fqn(raw).and_then(|fqn| php_key(&fqn)), false),
        }
    }

    /// Whether `raw` names the using class of the current trait body, which
    /// the trait's own key does not identify.
    fn names_trait_user(&self, raw: &str) -> bool {
        self.current_class().is_some_and(|class| class.trait_body)
            && (raw.eq_ignore_ascii_case("self") || raw.eq_ignore_ascii_case("static"))
    }

    /// The lookup for a class-like name, or for one of its members.
    pub(super) fn class_lookup(&self, raw: &str, member: Option<&str>) -> Option<String> {
        if member.is_none() && self.names_trait_user(raw) {
            return exact_lookup(Intent::Abstain, &[raw]);
        }
        let (Some(key), dispatch) = self.class_key(raw) else {
            return exact_lookup(Intent::Abstain, &[raw]);
        };
        let intent = Intent::scoped(member.is_some(), dispatch);
        match member {
            Some(member) => exact_lookup(intent, &[&key, KEY_SEPARATOR, member]),
            None => exact_lookup(intent, &[&key]),
        }
    }

    /// The lookup for a trait a class-like body uses; an adapted use is
    /// marked so the indexer knows the trait's methods may be renamed.
    pub(super) fn trait_use_lookup(&self, raw: &str, adapted: bool) -> Option<String> {
        if !adapted {
            return self.class_lookup(raw, None);
        }
        match self.class_fqn(raw).and_then(|fqn| php_key(&fqn)) {
            Some(key) => exact_lookup(Intent::AdaptedClass, &[&key]),
            None => exact_lookup(Intent::Abstain, &[raw]),
        }
    }

    /// The dispatch lookup for `$this->member` in the current class.
    pub(super) fn this_member_lookup(&self, member: &str) -> Option<String> {
        let key = self.current_class()?.key.as_deref()?;
        exact_lookup(Intent::DispatchMember, &[key, KEY_SEPARATOR, member])
    }

    /// The method a static call `raw::factory()` names, or `None` when its
    /// class cannot be determined exactly. The call is in the factory's own
    /// class when `raw` names the enclosing class (`self`, `static`, or its
    /// own name); `parent::` and any other class are outside it.
    pub(super) fn static_factory(&self, raw: &str, factory: &str) -> Option<FactoryCall> {
        let class = self.class_key(raw).0?;
        let own_class = self
            .current_class()
            .and_then(|current| current.key.as_deref())
            .is_some_and(|current| current.eq_ignore_ascii_case(&class));
        Some(FactoryCall {
            key: bounded_concat(&[&class, KEY_SEPARATOR, factory], MAX_LOOKUP_BYTES)?,
            own_class,
        })
    }

    /// The method `$this->factory()` names in the current class, or `None`
    /// outside a named class.
    pub(super) fn this_factory(&self, factory: &str) -> Option<FactoryCall> {
        let class = self.current_class()?.key.as_deref()?;
        Some(FactoryCall {
            key: bounded_concat(&[class, KEY_SEPARATOR, factory], MAX_LOOKUP_BYTES)?,
            own_class: true,
        })
    }

    /// The lookup for a called function name.
    pub(super) fn function_lookup(&self, raw: &str) -> Option<String> {
        if raw.contains('\\') {
            return match self.class_fqn(raw).and_then(|fqn| php_key(&fqn)) {
                Some(key) => exact_lookup(Intent::Function, &[&key]),
                None => exact_lookup(Intent::Abstain, &[raw]),
            };
        }
        if self.aliases_overflowed {
            return exact_lookup(Intent::Abstain, &[raw]);
        }
        if let Some(fqn) = self.functions.get(&raw.to_ascii_lowercase()) {
            return exact_lookup(Intent::Function, &[&php_key(fqn)?]);
        }
        match self.namespace.as_deref() {
            Some(namespace) => {
                exact_lookup(Intent::FunctionFallback, &[namespace, KEY_SEPARATOR, raw])
            }
            None => exact_lookup(Intent::Function, &[raw]),
        }
    }
}

/// A factory method a call receiver invokes.
pub(super) struct FactoryCall {
    /// Candidate key of the factory method (`Class::factory`).
    key: String,
    /// Whether the call is lexically inside the factory's own class, where
    /// PHP admits a private or protected factory.
    own_class: bool,
}

/// The lookup for `member` called on the object a factory method returns.
pub(super) fn returned_member_lookup(factory: &FactoryCall, member: &str) -> Option<String> {
    let intent = if factory.own_class {
        Intent::OwnReturnedMember
    } else {
        Intent::ReturnedMember
    };
    exact_lookup(intent, &[&factory.key, KEY_SEPARATOR, member])
}

/// The marked exact lookup for `intent` over the concatenated key parts.
pub(super) fn exact_lookup(intent: Intent, key: &[&str]) -> Option<String> {
    let mut parts = Vec::with_capacity(key.len().saturating_add(LOOKUP_HEADER_PARTS));
    parts.extend([PHP_EXACT_RESOLUTION_PREFIX, intent.marker(), KEY_SEPARATOR]);
    parts.extend_from_slice(key);
    bounded_concat(&parts, MAX_LOOKUP_BYTES)
}

/// The project candidate key for a fully qualified PHP name: the namespace
/// path, then `::`, then the short name (`App\Models\User` → `App\Models::User`).
pub(super) fn php_key(fqn: &str) -> Option<String> {
    if !valid_fqn(fqn) {
        return None;
    }
    match fqn.rsplit_once('\\') {
        Some((namespace, name)) => {
            bounded_concat(&[namespace, KEY_SEPARATOR, name], MAX_LOOKUP_BYTES)
        }
        None => bounded_concat(&[fqn], MAX_LOOKUP_BYTES),
    }
}

/// Validate a PHP name: optional leading `\` or `$`, then identifier
/// segments separated by single backslashes.
pub(super) fn php_name(raw: &str) -> Option<&str> {
    let name = raw.trim();
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        return None;
    }
    let body = name
        .strip_prefix('$')
        .or_else(|| name.strip_prefix('\\'))
        .unwrap_or(name);
    body.split('\\').all(valid_segment).then_some(name)
}

/// Whether `fqn` is a bounded fully qualified name without a leading backslash.
pub(super) fn valid_fqn(fqn: &str) -> bool {
    !fqn.is_empty() && fqn.len() <= MAX_NAME_BYTES && fqn.split('\\').all(valid_segment)
}

/// One PHP identifier: a letter, underscore, or non-ASCII byte, then
/// alphanumerics, underscores, or non-ASCII bytes.
pub(super) fn valid_segment(segment: &str) -> bool {
    let mut characters = segment.chars();
    characters
        .next()
        .is_some_and(|first| first == '_' || first.is_alphabetic() || !first.is_ascii())
        && characters.all(|character| {
            character == '_' || character.is_alphanumeric() || !character.is_ascii()
        })
}

/// Strip a case-insensitive ASCII prefix.
fn strip_ascii_prefix<'value>(value: &'value str, prefix: &str) -> Option<&'value str> {
    value
        .get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .and_then(|_| value.get(prefix.len()..))
}

/// Concatenate `parts` into a fresh string no longer than `maximum` bytes.
pub(super) fn bounded_concat(parts: &[&str], maximum: usize) -> Option<String> {
    let length = parts
        .iter()
        .try_fold(0_usize, |length, part| length.checked_add(part.len()))?;
    if length > maximum {
        return None;
    }
    let mut output = String::new();
    output.try_reserve_exact(length).ok()?;
    for part in parts {
        output.push_str(part);
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(intent: &str, key: &str) -> String {
        format!("{PHP_EXACT_RESOLUTION_PREFIX}{intent}::{key}")
    }

    #[test]
    fn candidate_keys_split_the_namespace_from_the_short_name() {
        assert_eq!(
            php_key(r"App\Models\User").as_deref(),
            Some(r"App\Models::User")
        );
        assert_eq!(php_key("Closure").as_deref(), Some("Closure"));
        assert_eq!(
            php_key(r"\Leading"),
            None,
            "keys come from fully qualified names"
        );
        assert_eq!(php_key(r"App\\User"), None, "empty segments are not names");
        assert_eq!(php_key("1Bad"), None);
    }

    #[test]
    fn class_names_follow_php_compile_time_resolution() {
        let mut scope = PhpScope::default();
        scope.enter_namespace(Some(r"App\Http".to_owned()), true);
        scope.register_alias(ImportKind::Class, "User", r"App\Models\User");
        scope.register_alias(ImportKind::Class, "M", r"Vendor\Models");
        let fqn = |raw: &str| scope.class_fqn(raw);
        assert_eq!(fqn("User").as_deref(), Some(r"App\Models\User"));
        assert_eq!(
            fqn(r"M\Profile").as_deref(),
            Some(r"Vendor\Models\Profile"),
            "a namespace alias expands before the key is split"
        );
        assert_eq!(
            fqn("user").as_deref(),
            Some(r"App\Models\User"),
            "aliases ignore case"
        );
        assert_eq!(fqn("Order").as_deref(), Some(r"App\Http\Order"));
        assert_eq!(fqn(r"\Order").as_deref(), Some("Order"));
        assert_eq!(fqn(r"namespace\User").as_deref(), Some(r"App\Http\User"));
        assert_eq!(
            scope.class_lookup(r"M\Profile", Some("find")),
            Some(lookup("member", r"Vendor\Models::Profile::find"))
        );

        scope.enter_namespace(Some("Other".to_owned()), true);
        assert_eq!(
            scope.class_fqn("User").as_deref(),
            Some(r"Other\User"),
            "imports never cross namespace blocks"
        );
    }

    #[test]
    fn function_names_use_their_own_import_table_and_runtime_fallback() {
        let mut scope = PhpScope::default();
        scope.enter_namespace(Some("App".to_owned()), true);
        scope.register_alias(ImportKind::Class, "helper", r"Vendor\Helper");
        scope.register_alias(ImportKind::Function, "format", r"App\Support\format");
        assert_eq!(
            scope.function_lookup("format"),
            Some(lookup("function", r"App\Support::format"))
        );
        assert_eq!(
            scope.function_lookup("helper"),
            Some(lookup("function-fallback", "App::helper")),
            "a class alias never captures a function call"
        );
        assert_eq!(
            scope.function_lookup(r"\strlen"),
            Some(lookup("function", "strlen"))
        );
        scope.enter_namespace(None, false);
        assert_eq!(
            scope.function_lookup("global_fn"),
            Some(lookup("function", "global_fn"))
        );
    }

    #[test]
    fn class_context_names_resolve_to_the_enclosing_and_parent_classes() {
        let mut scope = PhpScope::default();
        assert_eq!(
            scope.class_lookup("self", Some("make")),
            Some(lookup("abstain", "self")),
            "self outside a class has no target and must not be searched for"
        );
        scope.push_class(ClassContext {
            key: Some(r"App::Child".to_owned()),
            parent: Some(r"App::Base".to_owned()),
            trait_body: false,
        });
        assert_eq!(
            scope.class_lookup("self", Some("make")),
            Some(lookup("member", "App::Child::make"))
        );
        assert_eq!(
            scope.class_lookup("static", Some("make")),
            Some(lookup("dispatch-member", "App::Child::make")),
            "static:: is late-bound"
        );
        assert_eq!(
            scope.class_lookup("static", None),
            Some(lookup("dispatch-class", "App::Child"))
        );
        assert_eq!(
            scope.class_lookup("parent", Some("boot")),
            Some(lookup("member", "App::Base::boot"))
        );
        assert_eq!(
            scope.this_member_lookup("run"),
            Some(lookup("dispatch-member", "App::Child::run")),
            "an override can receive $this calls"
        );
        scope.pop_class();
        assert_eq!(scope.this_member_lookup("run"), None);
        scope.push_class(ClassContext {
            key: Some(r"App::Greets".to_owned()),
            parent: None,
            trait_body: true,
        });
        assert_eq!(
            scope.class_lookup("self", Some("hello")),
            Some(lookup("dispatch-member", "App::Greets::hello")),
            "self in a trait is the using class, so its members are late-bound"
        );
        assert_eq!(
            scope.class_lookup("self", None),
            Some(lookup("abstain", "self")),
            "a trait is never the class that `new self` constructs"
        );
    }

    #[test]
    fn alias_tables_stop_resolving_once_they_overflow() {
        let mut scope = PhpScope::default();
        scope.enter_namespace(Some("App".to_owned()), true);
        for index in 0..=MAX_ALIASES {
            scope.register_alias(
                ImportKind::Class,
                &format!("Alias{index}"),
                &format!(r"Vendor\Alias{index}"),
            );
        }
        assert!(scope.class_fqn("Unlisted").is_none());
        assert_eq!(
            scope.class_lookup("Unlisted", None),
            Some(lookup("abstain", "Unlisted")),
            "an unrecorded alias may own the name, so it is not namespace-relative"
        );
        assert_eq!(
            scope.function_lookup("unlisted"),
            Some(lookup("abstain", "unlisted"))
        );
        assert_eq!(
            scope.class_fqn(r"\Fully\Qualified").as_deref(),
            Some(r"Fully\Qualified")
        );
    }
}
