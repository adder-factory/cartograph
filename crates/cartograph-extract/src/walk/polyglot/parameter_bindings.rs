//! Names bound by the parameters of enclosing scopes, computed once per scope.
//!
//! A constant-shaped read is not a reference when a parameter, result,
//! receiver, or const generic parameter of an enclosing scope binds the same
//! name. Rescanning every enclosing parameter list for every read would make
//! per-read work grow with the parameter list, so each scope's binding names
//! are collected once, under one node budget, and later reads only look them
//! up. A scope whose binding sites exceed the budget is saturated: it is
//! treated as binding every name, so an unscanned binding is never mistaken
//! for a read.

use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

use crate::{ExtractError, walk::ExtractionBuilder};

/// Most binding-site nodes scanned for one scope; a scope with more binds every name.
const MAX_SCOPE_BINDING_NODES: usize = 4096;

/// The binding names of every scope checked so far in one file, by scope node.
#[derive(Default)]
pub(super) struct ParameterBindings {
    scopes: HashMap<usize, ScopeNames>,
}

/// The names one scope binds.
enum ScopeNames {
    /// The scope's binding sites exceeded the scan budget: every name is bound.
    Saturated,
    /// The relevant names the scope's binding sites declare.
    Names(HashSet<String>),
}

impl ScopeNames {
    fn binds(&self, name: &str) -> bool {
        match self {
            Self::Saturated => true,
            Self::Names(names) => names.contains(name),
        }
    }
}

/// Collects one scope's binding names under the shared scan budget.
#[derive(Default)]
pub(super) struct ScopeNameSet {
    names: HashSet<String>,
    scanned: usize,
    saturated: bool,
}

impl ScopeNameSet {
    /// Count one scanned binding-site node. Returns `false` once the scope has
    /// exceeded the budget, after which the collector must stop scanning.
    pub(super) fn scan(&mut self) -> bool {
        if self.saturated {
            return false;
        }
        self.scanned = self.scanned.saturating_add(1);
        self.saturated = self.scanned > MAX_SCOPE_BINDING_NODES;
        !self.saturated
    }

    /// Record a name the scope binds.
    pub(super) fn bind(&mut self, name: &str) -> Result<(), ExtractError> {
        if self.names.contains(name) {
            return Ok(());
        }
        let mut owned = String::new();
        owned
            .try_reserve_exact(name.len())
            .map_err(|_| ExtractError::OutputLimit)?;
        owned.push_str(name);
        self.names
            .try_reserve(1)
            .map_err(|_| ExtractError::OutputLimit)?;
        self.names.insert(owned);
        Ok(())
    }

    fn finish(self) -> ScopeNames {
        if self.saturated {
            ScopeNames::Saturated
        } else {
            ScopeNames::Names(self.names)
        }
    }
}

/// Collects a scope's binding names from the file source.
pub(super) type CollectScopeNames =
    fn(&str, Node<'_>, &mut ScopeNameSet) -> Result<(), ExtractError>;

/// Whether one scope binds one name.
#[derive(Clone, Copy)]
pub(super) struct ScopeQuery<'tree, 'name> {
    pub(super) scope: Node<'tree>,
    pub(super) name: &'name str,
    pub(super) collect: CollectScopeNames,
}

/// Whether `query.scope` binds `query.name`, collecting the scope's names on
/// its first query.
pub(super) fn scope_binds(
    builder: &mut ExtractionBuilder<'_, '_>,
    query: ScopeQuery<'_, '_>,
) -> Result<bool, ExtractError> {
    let key = query.scope.id();
    if let Some(names) = builder.polyglot.parameter_bindings.scopes.get(&key) {
        return Ok(names.binds(query.name));
    }
    builder.context.ensure_active()?;
    let mut names = ScopeNameSet::default();
    (query.collect)(builder.context.source(), query.scope, &mut names)?;
    let names = names.finish();
    let binds = names.binds(query.name);
    let scopes = &mut builder.polyglot.parameter_bindings.scopes;
    scopes
        .try_reserve(1)
        .map_err(|_| ExtractError::OutputLimit)?;
    scopes.insert(key, names);
    Ok(binds)
}

/// The trimmed source text of a node.
pub(super) fn node_text<'source>(source: &'source str, node: Node<'_>) -> &'source str {
    source
        .get(node.start_byte()..node.end_byte())
        .unwrap_or_default()
        .trim()
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, fmt::Write as _};

    use tree_sitter::Parser;

    use super::*;
    use crate::{
        DEFAULT_MAXIMUM_AST_DEPTH, NativeGrammar, SourceLimits, SourceSnapshot,
        walk::syntax::descendants_including_root,
    };

    /// Const generic parameters of the wide scope.
    const WIDE_PARAMETERS: usize = 3_000;
    /// Constant reads inside the wide scope.
    const READS: usize = 3_000;

    thread_local! {
        static COLLECTIONS: Cell<usize> = const { Cell::new(0) };
    }

    /// Counts its calls and binds every const generic name of the scope.
    fn counting_collect(
        source: &str,
        scope: Node<'_>,
        names: &mut ScopeNameSet,
    ) -> Result<(), ExtractError> {
        COLLECTIONS.with(|count| count.set(count.get().saturating_add(1)));
        for parameter in descendants_including_root(scope)
            .filter(|node| node.kind() == "const_parameter")
            .filter_map(|parameter| parameter.child_by_field_name("name"))
        {
            if !names.scan() {
                return Ok(());
            }
            names.bind(node_text(source, parameter))?;
        }
        Ok(())
    }

    #[test]
    fn each_scope_is_collected_once_however_many_reads_it_encloses() {
        let mut source = String::from("fn wide<");
        for index in 0..WIDE_PARAMETERS {
            assert!(write!(&mut source, "const P{index}: usize, ").is_ok());
        }
        source.push_str(">() {\n");
        for _ in 0..READS {
            source.push_str("    let _ = LIMIT;\n");
        }
        source.push_str("}\nfn shadow<const LIMIT: usize>() -> usize { LIMIT }\n");
        let limits = SourceLimits::new(source.len())
            .unwrap_or_else(|error| panic!("test source bound is invalid: {error}"));
        let snapshot = SourceSnapshot::from_bytes("src/wide.rs", source.as_bytes(), limits)
            .unwrap_or_else(|error| panic!("test snapshot is invalid: {error}"));
        let mut parser = Parser::new();
        parser
            .set_language(&NativeGrammar::Rust.language())
            .unwrap_or_else(|error| panic!("test grammar setup failed: {error}"));
        let tree = parser
            .parse(snapshot.source(), None)
            .unwrap_or_else(|| panic!("test parser did not produce a tree"));
        let mut cancelled = || false;
        let mut builder =
            ExtractionBuilder::new(&snapshot, DEFAULT_MAXIMUM_AST_DEPTH, &mut cancelled)
                .unwrap_or_else(|error| panic!("test extraction builder failed: {error}"));
        COLLECTIONS.with(|count| count.set(0));

        let mut bound = Vec::new();
        for read in descendants_including_root(tree.root_node()).filter(|node| {
            node.kind() == "identifier" && node_text(source.as_str(), *node) == "LIMIT"
        }) {
            let scope = std::iter::successors(read.parent(), Node::parent)
                .find(|node| node.kind() == "function_item")
                .unwrap_or_else(|| panic!("every read is inside a function"));
            let query = ScopeQuery {
                scope,
                name: "LIMIT",
                collect: counting_collect,
            };
            bound.push(
                scope_binds(&mut builder, query)
                    .unwrap_or_else(|error| panic!("scope lookup failed: {error}")),
            );
        }

        assert_eq!(COLLECTIONS.with(Cell::get), 2, "one collection per scope");
        assert_eq!(bound.iter().filter(|bound| !**bound).count(), READS);
        assert_eq!(bound.iter().filter(|bound| **bound).count(), 2);
    }
}
