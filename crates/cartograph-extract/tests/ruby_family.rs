//! Ruby extraction contracts restored from the v1 Ruby extractor.

mod credential_support;
mod dependency_ownership;
mod script_family_support;

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolKind, Visibility};
use cartograph_extract::{ExtractedFile, ImportBindingKind};
use script_family_support::{
    ReferenceQuery, assert_linear_work, extract, names_of_kind, numbered_names, symbol,
};

const SECRET_SENTINEL: &str = "sk_live_ruby_family_secret";

#[test]
fn ruby_reopened_inherited_singleton_receivers_do_not_claim_an_owner() {
    let extracted = extract(
        "lib/reopened.rb",
        "class A; end; class Parent; class A; end; end; class B < Parent; end; class B; def A.run; end; end\n",
    );
    assert_unowned_method(&extracted, "run");
}

#[test]
fn ruby_eigenclass_constants_do_not_prove_singleton_owners() {
    let extracted = extract(
        "lib/eigenclass.rb",
        "class A; class << self; class Hidden; end; end; end; class B; class << A::Hidden; def run; end; end; end\n",
    );
    assert!(!extracted.symbols.iter().any(|symbol| symbol.name == "run"));
}

#[test]
fn ruby_reassigned_singleton_receivers_do_not_claim_the_old_class() {
    let extracted = extract(
        "lib/reassigned.rb",
        "module Outer; class A; end; end; class Parent; end; class B < Parent; Outer::A = Object.new; end; class << ::Outer::A; def run; end; end\n",
    );
    assert!(!extracted.symbols.iter().any(|symbol| symbol.name == "run"));
}

fn assert_unowned_method(extracted: &ExtractedFile, name: &str) {
    let method = symbol(extracted, SymbolKind::Method, name);
    assert_eq!(method.qualified_name, name);
    assert!(!method.export.exported);
    assert!(
        !extracted
            .containments
            .iter()
            .any(|edge| edge.child == method.id)
    );
}

#[test]
fn ruby_singleton_methods_claim_only_enclosing_owners() {
    let extracted = extract(
        "lib/singletons.rb",
        "class A; end; class B; def A.run; helper(); end; def B.named; named_helper(); end; def self.own; own_helper(); end; def (factory()).computed; computed_helper(); end; end; module M; def M.module_named; end; end\n",
    );
    let b = symbol(&extracted, SymbolKind::Class, "B");
    let run = symbol(&extracted, SymbolKind::Method, "run");
    assert_unowned_method(&extracted, "run");
    assert_unowned_method(&extracted, "computed");
    let named = symbol(&extracted, SymbolKind::Method, "named");
    assert_eq!(named.qualified_name, "B::named");
    assert!(
        extracted
            .containments
            .iter()
            .any(|edge| edge.parent == b.id && edge.child == named.id)
    );
    assert!(call_names(&extracted, &run.id).contains(&"helper"));
    assert_eq!(
        symbol(&extracted, SymbolKind::Method, "own").qualified_name,
        "B::own"
    );
    assert!(call_names(&extracted, &b.id).contains(&"factory"));
    let computed = symbol(&extracted, SymbolKind::Method, "computed");
    assert!(call_names(&extracted, &computed.id).contains(&"computed_helper"));
    assert!(!call_names(&extracted, &computed.id).contains(&"factory"));
    assert_eq!(
        symbol(&extracted, SymbolKind::Method, "module_named").qualified_name,
        "M::module_named"
    );
}

#[test]
fn ruby_requires_become_imports_with_exact_bindings_and_no_require_calls() {
    let extracted = extract(
        "lib/app.rb",
        "require 'json'\nrequire 'active_support/core_ext/string'\nrequire_relative '../test_helper'\nrequire_relative 'helper'\nrequire './local'\nputs 'hello'\nrequire \"lib/#{name}\"\n",
    );
    assert_eq!(extracted.language, SourceLanguage::Ruby);
    let imports = names_of_kind(&extracted, SymbolKind::Import);
    assert_eq!(
        imports,
        [
            "../test_helper",
            "./local",
            "active_support/core_ext/string",
            "helper",
            "json"
        ]
    );
    for (specifier, kind) in [
        ("json", ImportBindingKind::IncludeSystem),
        (
            "active_support/core_ext/string",
            ImportBindingKind::IncludeSystem,
        ),
        ("../test_helper", ImportBindingKind::Namespace),
        ("./helper", ImportBindingKind::Namespace),
        ("./local", ImportBindingKind::Namespace),
    ] {
        assert!(
            ReferenceQuery::new(ReferenceKind::Imports, specifier).found_in(&extracted),
            "missing import reference {specifier}: {:?}",
            extracted.references
        );
        let binding = extracted
            .import_bindings
            .iter()
            .find(|binding| binding.module_specifier == specifier)
            .unwrap_or_else(|| panic!("missing binding {specifier}"));
        assert_eq!(binding.kind, kind, "{specifier}");
        assert_eq!(binding.imported_name, "*");
        assert_eq!(
            binding.local_name, "<load>",
            "a load binds no identifier, so its binding must not claim any reference name"
        );
    }
    let require_calls = extracted
        .references
        .iter()
        .filter(|reference| {
            reference.kind == ReferenceKind::Calls && reference.name.starts_with("require")
        })
        .map(|reference| reference.span.start_line())
        .collect::<Vec<_>>();
    assert_eq!(
        require_calls,
        [7],
        "literal loads are imports; only the dynamic require stays a call"
    );
    assert!(ReferenceQuery::new(ReferenceKind::Calls, "puts").found_in(&extracted));
    assert_eq!(
        extracted.import_bindings.len(),
        5,
        "interpolated requires are dynamic and must not be guessed"
    );
}

#[test]
fn ruby_modules_classes_and_free_methods_keep_v1_kinds_and_nesting() {
    let extracted = extract(
        "lib/auth.rb",
        "module Discourse\n  module Auth\n    class AuthProvider < Base::Provider\n      def authenticate(params)\n        validate(params)\n      end\n      def self.disable\n      end\n    end\n  end\nend\nmodule Outer::Inner\nend\ndef free_helper(value)\n  value\nend\n",
    );
    let provider = symbol(&extracted, SymbolKind::Class, "AuthProvider");
    assert_eq!(provider.qualified_name, "Discourse::Auth::AuthProvider");
    assert!(
        !provider.export.exported,
        "a nested constant is namespaced, so its bare name must not resolve project-wide"
    );
    assert!(
        symbol(&extracted, SymbolKind::Module, "Discourse")
            .export
            .exported
    );
    let method = symbol(&extracted, SymbolKind::Method, "authenticate");
    assert_eq!(
        method.qualified_name,
        "Discourse::Auth::AuthProvider::authenticate"
    );
    assert!(!method.export.exported);
    let singleton = symbol(&extracted, SymbolKind::Method, "disable");
    assert!(singleton.execution.static_member);
    assert_eq!(
        symbol(&extracted, SymbolKind::Module, "Outer::Inner").qualified_name,
        "Outer::Inner"
    );
    let free = symbol(&extracted, SymbolKind::Function, "free_helper");
    assert!(free.export.exported);
    assert!(
        !extracted
            .symbols
            .iter()
            .any(|symbol| symbol.kind == SymbolKind::Method && symbol.name == "free_helper"),
        "a def outside any class or module is a function"
    );
    assert!(
        ReferenceQuery::new(ReferenceKind::Inherits, "Base::Provider")
            .owned_by(&provider.id)
            .found_in(&extracted)
    );
    assert!(
        ReferenceQuery::new(ReferenceKind::Calls, "validate")
            .owned_by(&method.id)
            .found_in(&extracted)
    );
}

#[test]
fn ruby_accessor_macros_emit_one_field_per_symbol_only_inside_classes() {
    let extracted = extract(
        "app/models/user.rb",
        "class User\n  attr_reader :name, :email\n  attr_writer :role\n  attr_accessor :timestamp\n  class_attribute :default_scope_value, instance_writer: false\n  validates :title, presence: true\n  before_action :authenticate\n  has_many :comments\n  def call; end\nend\nattr_reader :stray\n",
    );
    let class = symbol(&extracted, SymbolKind::Class, "User");
    assert_eq!(
        names_of_kind(&extracted, SymbolKind::Field),
        ["default_scope_value", "email", "name", "role", "timestamp"]
    );
    for field in extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Field)
    {
        assert!(
            extracted
                .containments
                .iter()
                .any(|edge| edge.parent == class.id && edge.child == field.id),
            "missing containment for {}",
            field.name
        );
        assert_eq!(field.visibility, Some(Visibility::Public));
    }
    assert!(
        !extracted
            .references
            .iter()
            .any(|reference| reference.name.starts_with("attr_")
                && reference.owner.as_ref() == Some(&class.id)),
        "accessor macros inside a class declare fields rather than calls"
    );
    assert!(
        ReferenceQuery::new(ReferenceKind::Calls, "attr_reader").found_in(&extracted),
        "a top-level accessor macro declares no field and stays an ordinary call"
    );
}

#[test]
fn ruby_constants_and_top_level_variables_follow_v1_scoping() {
    let extracted = extract(
        "lib/base.rb",
        "MODULES = [:foo, :bar]\nPROTECTED_IVARS = AbstractController::Rendering::DEFAULT_PROTECTED_INSTANCE_VARIABLES.freeze\nlocal = compute(1)\nclass Base\n  DEFAULT_TIMEOUT = 30\n  ignored = 1\n  def call\n    scoped = 2\n  end\nend\n",
    );
    assert_eq!(
        names_of_kind(&extracted, SymbolKind::Constant),
        ["DEFAULT_TIMEOUT", "MODULES", "PROTECTED_IVARS"]
    );
    assert_eq!(names_of_kind(&extracted, SymbolKind::Variable), ["local"]);
    let class = symbol(&extracted, SymbolKind::Class, "Base");
    let timeout = symbol(&extracted, SymbolKind::Constant, "DEFAULT_TIMEOUT");
    assert_eq!(timeout.qualified_name, "Base::DEFAULT_TIMEOUT");
    assert!(
        timeout.signature.is_none(),
        "numeric literals never enter signatures"
    );
    assert!(
        extracted
            .containments
            .iter()
            .any(|edge| edge.parent == class.id && edge.child == timeout.id)
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Constant, "PROTECTED_IVARS")
            .signature
            .as_deref(),
        Some("= AbstractController::Rendering::DEFAULT_PROTECTED_INSTANCE_VARIABLES.freeze")
    );
    assert!(
        symbol(&extracted, SymbolKind::Constant, "MODULES")
            .export
            .exported
    );
    assert!(!timeout.export.exported);
    let local = symbol(&extracted, SymbolKind::Variable, "local");
    assert!(!local.export.exported);
    assert!(
        ReferenceQuery::new(ReferenceKind::Calls, "compute")
            .owned_by(&local.id)
            .found_in(&extracted)
    );
}

#[test]
fn ruby_statement_identifiers_and_receivers_name_calls_like_v1() {
    let extracted = extract(
        "app/services/invoice.rb",
        "class Invoice\n  def calc(param)\n    reset\n    Factory.run()\n    Repository.new.save\n    Repository.new\n    Billing::Ledger.post(1)\n    @items.length\n    self.helper\n    total = 1\n    total\n    param\n    nil\n    Constant\n    items.each do |item|\n      item\n      cleanup\n    end\n    if total\n      flush\n    end\n    begin\n      attempt\n    rescue StandardError\n      report\n    ensure\n      finish\n    end\n  end\nend\n",
    );
    let calc = symbol(&extracted, SymbolKind::Method, "calc");
    let calls = call_names(&extracted, &calc.id);
    for expected in [
        "reset",
        "Factory.run",
        "Repository.save",
        "Repository.new",
        "Billing::Ledger.post",
        "@items.length",
        "helper",
        "items.each",
        "cleanup",
        "flush",
        "attempt",
        "report",
        "finish",
    ] {
        assert!(
            calls.contains(&expected),
            "missing call {expected}; calls={calls:?}"
        );
    }
    for absent in ["total", "param", "item", "nil", "Constant", "run", "save"] {
        assert!(
            !calls.contains(&absent),
            "{absent} must not be a call; calls={calls:?}"
        );
    }
}

#[test]
fn ruby_locals_follow_source_order_and_block_scopes() {
    let extracted = extract(
        "lib/order.rb",
        "class Order\n  def run(items)\n    reset\n    reset = 1\n    reset\n    copy = copy\n    copy\n    items.each do |item|\n      inner = item\n      inner\n    end\n    inner\n    tally ||= 0\n    tally\n    begin\n      attempt\n    rescue StandardError => error\n      error\n    end\n  end\n  def other\n    reset\n  end\nend\n",
    );
    let run = symbol(&extracted, SymbolKind::Method, "run");
    let calls = call_names(&extracted, &run.id);
    assert_eq!(
        ReferenceQuery::new(ReferenceKind::Calls, "reset")
            .owned_by(&run.id)
            .count_in(&extracted),
        1,
        "only the read before the assignment is a call: {calls:?}"
    );
    assert_eq!(
        calls.iter().filter(|name| **name == "inner").count(),
        1,
        "a block-local disappears after its block: {calls:?}"
    );
    for local in ["copy", "items", "item", "tally", "error"] {
        assert!(!calls.contains(&local), "{local} is a local: {calls:?}");
    }
    assert!(calls.contains(&"attempt"));
    let other = symbol(&extracted, SymbolKind::Method, "other");
    assert_eq!(
        call_names(&extracted, &other.id),
        ["reset"],
        "each method starts a fresh local scope"
    );
}

#[test]
fn ruby_visibility_sections_apply_to_following_instance_methods() {
    let extracted = extract(
        "app/models/account.rb",
        "class Account\n  def initialize; end\n  def opened; end\n  private def hidden; end\n  def still_public; end\n  def later_private; end\n  private :later_private\n  def self.factory; end\n  private_class_method :factory\n  private\n  attr_reader :token\n  def secret; end\n  def self.build; end\n  protected\n  def guarded; end\n  public\n  def reopened; end\n  class << self\n    private\n    def hidden_singleton; end\n  end\n  def after_singleton; end\nend\n",
    );
    for (name, expected) in [
        ("initialize", Visibility::Private),
        ("opened", Visibility::Public),
        ("hidden", Visibility::Private),
        ("still_public", Visibility::Public),
        ("later_private", Visibility::Private),
        ("factory", Visibility::Private),
        ("hidden_singleton", Visibility::Private),
        ("after_singleton", Visibility::Public),
        ("secret", Visibility::Private),
        ("build", Visibility::Public),
        ("guarded", Visibility::Protected),
        ("reopened", Visibility::Public),
    ] {
        assert_eq!(
            symbol(&extracted, SymbolKind::Method, name).visibility,
            Some(expected),
            "{name}"
        );
    }
    assert_eq!(
        symbol(&extracted, SymbolKind::Field, "token").visibility,
        Some(Visibility::Private)
    );
    assert!(
        !extracted.references.iter().any(|reference| matches!(
            reference.name.as_str(),
            "private" | "protected" | "public" | "private_class_method"
        )),
        "section modifiers are not calls: {:?}",
        extracted.references
    );
}

#[test]
fn ruby_extraction_is_deterministic_and_literal_free() {
    let source = format!(
        "TOKEN = '{SECRET_SENTINEL}'\nSYMBOL = :{SECRET_SENTINEL}\nWORDS = %w[{SECRET_SENTINEL}]\nclass Vault\n  KEY = \"{SECRET_SENTINEL}\"\n  def open(secret = '{SECRET_SENTINEL}')\n    unlock('{SECRET_SENTINEL}')\n  end\n  def keyed(token: :{SECRET_SENTINEL}); end\n  def quoted(token = %q({SECRET_SENTINEL})); end\nend\n"
    );
    let first = extract("lib/vault.rb", &source);
    let second = extract("lib/vault.rb", &source);
    assert_eq!(first, second);
    assert!(!format!("{first:?}").contains(SECRET_SENTINEL));
    for method in ["open", "keyed", "quoted"] {
        assert!(
            symbol(&first, SymbolKind::Method, method)
                .signature
                .is_none(),
            "{method} defaults are literals"
        );
    }
    for constant in ["TOKEN", "SYMBOL", "WORDS"] {
        assert!(
            symbol(&first, SymbolKind::Constant, constant)
                .signature
                .is_none()
        );
    }
}

#[test]
fn ruby_singleton_contexts_and_explicit_wrappers_own_their_visibility() {
    let extracted = extract(
        "lib/factory.rb",
        "class Factory\n  def make; end\n  def build; end\n  public def initialize; end\n  class << self\n    def make; end\n    private :make\n    private def hidden; end\n    if true\n      def build; end\n      private :build\n    end\n  end\nend\n",
    );
    let methods = extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Method && symbol.name == "make")
        .map(|symbol| (symbol.execution.static_member, symbol.visibility))
        .collect::<Vec<_>>();
    assert_eq!(
        methods,
        [
            (false, Some(Visibility::Public)),
            (true, Some(Visibility::Private))
        ],
        "`private :make` inside `class << self` restricts the singleton method only"
    );
    let builds = extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Method && symbol.name == "build")
        .map(|symbol| (symbol.execution.static_member, symbol.visibility))
        .collect::<Vec<_>>();
    assert_eq!(
        builds,
        [
            (false, Some(Visibility::Public)),
            (true, Some(Visibility::Private))
        ],
        "singleton context survives conditionals"
    );
    let hidden = symbol(&extracted, SymbolKind::Method, "hidden");
    assert!(hidden.execution.static_member);
    assert_eq!(hidden.visibility, Some(Visibility::Private));
    assert_eq!(
        symbol(&extracted, SymbolKind::Method, "initialize").visibility,
        Some(Visibility::Public),
        "an explicit wrapper overrides implicit privacy"
    );
}

#[test]
fn ruby_declarations_keep_their_executable_children_and_chained_constants() {
    let extracted = extract(
        "app/models/settings.rb",
        "class Settings\n  class_attribute :settings, default: build_defaults()\n  class << registry()\n    def tracked; end\n  end\nend\nPRIMARY = SECONDARY = compute()\n",
    );
    let class = symbol(&extracted, SymbolKind::Class, "Settings");
    for name in ["build_defaults", "registry"] {
        assert!(
            ReferenceQuery::new(ReferenceKind::Calls, name)
                .owned_by(&class.id)
                .found_in(&extracted),
            "{name} still executes: {:?}",
            extracted.references
        );
    }
    assert_eq!(names_of_kind(&extracted, SymbolKind::Field), ["settings"]);
    let secondary = symbol(&extracted, SymbolKind::Constant, "SECONDARY");
    assert_eq!(secondary.qualified_name, "SECONDARY");
    assert!(secondary.export.exported);
    assert!(
        ReferenceQuery::new(ReferenceKind::Calls, "compute")
            .owned_by(&secondary.id)
            .found_in(&extracted)
    );
}

#[test]
fn ruby_computed_receivers_never_put_arguments_into_call_names() {
    let source = format!(
        "def run\n  factory(:{SECRET_SENTINEL})::Inner.go()\n  pick(:{SECRET_SENTINEL}).go\n  Outer::Inner.go\nend\n"
    );
    let extracted = extract("lib/computed.rb", &source);
    assert!(!format!("{extracted:?}").contains(SECRET_SENTINEL));
    let run = symbol(&extracted, SymbolKind::Function, "run");
    let calls = call_names(&extracted, &run.id);
    for expected in ["factory", "pick", "pick.go", "Outer::Inner.go"] {
        assert!(calls.contains(&expected), "missing {expected}: {calls:?}");
    }
}

#[test]
fn ruby_block_visibility_sections_stay_inside_their_block() {
    let extracted = extract(
        "app/models/concerns/searchable.rb",
        "module Searchable\n  class_methods do\n    def search; end\n    private\n    def build_query; end\n  end\n  def reindex; end\nend\nclass Ledger\n  included do\n    private\n  end\n  def open_entry; end\n  scoped { private }\n  def still_open; end\n  private\n  configure do\n    def configured; end\n  end\n  def hidden_entry; end\nend\n",
    );
    for (name, expected) in [
        ("search", Visibility::Public),
        ("build_query", Visibility::Private),
        ("reindex", Visibility::Public),
        ("open_entry", Visibility::Public),
        ("still_open", Visibility::Public),
        ("configured", Visibility::Public),
        ("hidden_entry", Visibility::Private),
    ] {
        assert_eq!(
            symbol(&extracted, SymbolKind::Method, name).visibility,
            Some(expected),
            "{name}"
        );
    }
}

#[test]
fn ruby_retroactive_privacy_reaches_reopened_classes_but_not_later_definitions() {
    let extracted = extract(
        "app/models/report.rb",
        "class Report\n  def render; end\n  def export; end\nend\nclass Report\n  private :render\n  def export; end\n  private :export\n  def render; end\nend\n",
    );
    let visibilities = |name: &str| {
        extracted
            .symbols
            .iter()
            .filter(|symbol| symbol.kind == SymbolKind::Method && symbol.name == name)
            .map(|symbol| symbol.visibility)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        visibilities("render"),
        [Some(Visibility::Private), Some(Visibility::Public)]
    );
    assert_eq!(
        visibilities("export"),
        [Some(Visibility::Private), Some(Visibility::Private)]
    );
}

#[test]
fn ruby_latest_retroactive_restriction_wins_for_every_earlier_definition() {
    let extracted = extract(
        "app/models/toggle.rb",
        "class Toggle
  def flip; end
  private :flip
  def flip; end
  public :flip, :flip
  def flip; end
  protected
  def flop; end
  public :flop
  private :flop
end
",
    );
    let visibilities = |name: &str| {
        extracted
            .symbols
            .iter()
            .filter(|symbol| symbol.kind == SymbolKind::Method && symbol.name == name)
            .map(|symbol| symbol.visibility)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        visibilities("flip"),
        [
            Some(Visibility::Public),
            Some(Visibility::Public),
            Some(Visibility::Public)
        ]
    );
    assert_eq!(visibilities("flop"), [Some(Visibility::Private)]);
}

#[test]
fn ruby_parenthesized_assignment_chains_declare_side_by_side() {
    let extracted = extract(
        "config/limits.rb",
        "A = (B = (C = 1))\nD = (\n  # documented link\n  E = 1\n)\n",
    );
    for name in ["A", "B", "C", "D", "E"] {
        assert_eq!(
            symbol(&extracted, SymbolKind::Constant, name).qualified_name,
            name,
            "{name} is declared at top level, not inside the constant it initializes"
        );
    }
}

#[test]
fn ruby_pattern_matching_captures_are_locals_not_calls() {
    let extracted = extract(
        "lib/matcher.rb",
        "def route(event)\n  case event\n  in [first, *rest]\n    first\n    rest\n  in {name:, size: Integer => size}\n    name\n    size\n  in ^pinned\n    unbound\n  end\n  event => {kind:}\n  kind\n  (begin; current; end) => current\n  current\n  (begin; latest; end) in latest\n  latest\nend\n",
    );
    let route = symbol(&extracted, SymbolKind::Function, "route");
    let calls = call_names(&extracted, &route.id);
    // The subject runs before its pattern binds the same name.
    for subject in ["current", "latest"] {
        assert_eq!(
            calls.iter().filter(|call| **call == subject).count(),
            1,
            "{subject} is called once as the subject: {calls:?}"
        );
    }
    for local in ["first", "rest", "name", "size", "kind"] {
        assert!(
            !calls.contains(&local),
            "{local} is bound by a pattern: {calls:?}"
        );
    }
    assert!(calls.contains(&"unbound"), "{calls:?}");
}

#[test]
fn ruby_wide_declarations_and_chains_take_linear_work() {
    assert_linear_work("app/models/wide.rb", |width| {
        format!(
            "class Wide\n  attr_reader {}\nend\n",
            numbered_names(":field", width, ", ")
        )
    });
    assert_linear_work("config/chain.rb", |width| {
        format!("{} = 1\n", numbered_names("LINK", width, " = "))
    });
    assert_linear_work("config/nested_chain.rb", |width| {
        format!(
            "{} = 1{}\n",
            numbered_names("NEST", width, " = ("),
            ")".repeat(width - 1)
        )
    });
}

fn call_names<'file>(
    extracted: &'file ExtractedFile,
    owner: &cartograph_domain::SymbolId,
) -> Vec<&'file str> {
    extracted
        .references
        .iter()
        .filter(|reference| {
            reference.kind == ReferenceKind::Calls && reference.owner.as_ref() == Some(owner)
        })
        .map(|reference| reference.name.as_str())
        .collect()
}

#[test]
fn ruby_loads_screen_credentials_before_emitting_import_facts() {
    credential_support::assert_screened(
        "main.rb",
        "require \"@VALUE@\"\nrequire_relative \"@VALUE@\"\nload \"@VALUE@\"\n",
        "token",
    );
}

#[test]
fn ruby_accessor_symbol_literals_screen_provider_keys() {
    for name in ["sk_live_FAKE1234567890abcdef", "ghp_aaaaaaaaaaaaaaaaaaaa"] {
        let source = format!("class Example; attr_reader :{name}; end\n");
        let file = credential_support::extract("accessors.rb", &source);
        credential_support::assert_no_credentials(&file);
        assert!(
            file.symbols
                .iter()
                .all(|symbol| symbol.kind != SymbolKind::Field)
        );
    }
    let file =
        credential_support::extract("accessors.rb", "class Example; attr_reader :token; end\n");
    assert!(
        file.symbols
            .iter()
            .any(|symbol| symbol.name == "token" && symbol.kind == SymbolKind::Field)
    );
}
