//! Ruby walk state kept in the scripting families' state slot.

use std::collections::BTreeMap;

use cartograph_domain::Visibility;

use super::locals::RubyLocals;

/// Methods already emitted under one qualified name and staticness.
type MethodKey = (String, bool);

/// Source-order state of the Ruby file being walked.
#[derive(Default)]
pub(in crate::walk) struct RubyState {
    /// Local-variable scopes, bound in source order.
    pub(super) locals: RubyLocals,
    /// Whether the innermost definition scope is a `class << self` body.
    pub(super) singleton_body: bool,
    /// A method definition or unmodelled mutation withholds builtin include proof.
    pub(super) include_blocked: bool,
    /// Every method emitted so far, by qualified name and staticness, with the
    /// latest retroactive restriction (`private :name`) naming it.
    methods: BTreeMap<MethodKey, MethodDefinitions>,
}

/// Definitions of one method name and the latest restriction applied to them.
#[derive(Default)]
struct MethodDefinitions {
    /// Indexes into the emitted symbols, in source order.
    indexes: Vec<usize>,
    /// The latest restriction: how many definitions existed when it ran, and
    /// the visibility it set.
    restriction: Option<(usize, Visibility)>,
}

/// One retroactive restriction of the definitions emitted so far.
pub(super) struct MethodRestriction {
    pub(super) qualified_name: String,
    pub(super) static_member: bool,
    pub(super) visibility: Visibility,
}

impl RubyState {
    /// Remember that the symbol at `index` is a method named `qualified_name`.
    pub(super) fn record_method(
        &mut self,
        qualified_name: String,
        static_member: bool,
        index: usize,
    ) {
        self.methods
            .entry((qualified_name, static_member))
            .or_default()
            .indexes
            .push(index);
    }

    /// Restrict every definition emitted so far under the restriction's name.
    ///
    /// Definitions only accumulate, so the latest restriction covers every
    /// definition an earlier one covered and overrides it; keeping only the
    /// latest makes each restriction constant work, and
    /// [`Self::take_restrictions`] applies them once after the walk.
    pub(super) fn restrict(&mut self, restriction: MethodRestriction) {
        let key = (restriction.qualified_name, restriction.static_member);
        if let Some(definitions) = self
            .methods
            .get_mut(&key)
            .filter(|definitions| !definitions.indexes.is_empty())
        {
            definitions.restriction = Some((definitions.indexes.len(), restriction.visibility));
        }
    }

    /// Every restricted symbol index with the visibility its latest
    /// restriction set, in key order; the method index is emptied.
    pub(super) fn take_restrictions(&mut self) -> Vec<(usize, Visibility)> {
        std::mem::take(&mut self.methods)
            .into_values()
            .filter_map(|definitions| {
                definitions
                    .restriction
                    .map(|(covered, visibility)| (definitions.indexes, covered, visibility))
            })
            .flat_map(|(indexes, covered, visibility)| {
                indexes
                    .into_iter()
                    .take(covered)
                    .map(move |index| (index, visibility))
            })
            .collect()
    }
}
