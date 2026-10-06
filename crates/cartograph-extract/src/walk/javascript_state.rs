//! Per-file JavaScript-family state shared between the walk and its passes.

use std::collections::BTreeSet;

use cartograph_domain::SymbolId;

use super::{
    javascript_owners::OwnerIndex, javascript_scopes::LexicalScopes,
    type_contracts::ContractGenerics,
};

/// What the JavaScript-family passes of one file remember between nodes.
#[derive(Default)]
pub(super) struct JavaScriptState<'source> {
    /// Contract generics this file declares (an object shape with a member
    /// typed by its string name parameter); their first string-literal
    /// argument names a contract property.
    pub(super) contract_generics: ContractGenerics,
    /// The scope index reads consult for their nearest enclosing binding.
    pub(super) scopes: LexicalScopes<'source>,
    /// The interval index shared by value-reference and binding-table reads.
    pub(super) owners: Option<OwnerIndex>,
    /// Owners whose declaration already recorded every type named anywhere
    /// inside it (a non-callable variable declarator, a type alias), so a type
    /// consumer in its value is not recorded a second time.
    pub(super) whole_type_owners: BTreeSet<SymbolId>,
}
