use super::{
    CanonicalGenerationFacts, CapabilityReferenceQuery, EXTERNAL_REFERENCE_UNRESOLVED_PROVENANCE,
    ReferenceKind, UNRESOLVED_IMPORT_PROVENANCE, assert_import_targets_file,
    build_capability_generation, capability_file_symbol, capability_symbol,
};

const RUBY_FIXTURES: [(&str, &str); 6] = [
    (
        "app/main.rb",
        "require_relative 'lib/helper'\nrequire 'lib/helper'\nrequire 'config/environment'\nrequire 'json'\nrequire 'missing/feature'\ndef run\n  shared_tool\n  json\nend\n",
    ),
    ("app/lib/helper.rb", "module AppHelper\nend\n"),
    ("lib/helper.rb", "module RootHelper\nend\n"),
    ("config/environment.rb", "BOOTED = 1\n"),
    ("app/tools.rb", "def shared_tool\nend\ndef json\nend\n"),
    ("app/json.rb", "def parse\nend\n"),
];

const LUA_FIXTURES: [(&str, &str); 7] = [
    (
        "main.lua",
        "local utils = require(\"app.utils\")\nlocal net = require 'app.net'\nlocal json = require(\"json\")\nrequire(\"app.missing\")\nlocal codec = require(\"lib/codec\")\nrequire(\"./lib/codec\")\nfunction start()\n  helper_global()\n  utils.pack()\nend\n",
    ),
    (
        "app/utils.lua",
        "local M = {}\nfunction pack() end\nfunction M.pack() end\nreturn M\n",
    ),
    ("app/utils/init.lua", "return {}\n"),
    ("app/net/init.lua", "return {}\n"),
    ("lib/globals.lua", "function helper_global()\nend\n"),
    ("app/json.lua", "return {}\n"),
    ("lib/codec.lua", "return {}\n"),
];

const NIX_FIXTURES: [(&str, &str); 6] = [
    (
        "default.nix",
        "{ pkgs ? import <nixpkgs> {} }:\n{\n  a = import ./pkgs/a.nix;\n  b = import ./lib;\n  c = import ./missing.nix;\n  d = import ./other;\n}\n",
    ),
    ("pkgs/a.nix", "{ a = 1; }\n"),
    ("lib/default.nix", "{ lib = 1; }\n"),
    ("other.nix", "{ other = 1; }\n"),
    (
        "nix/main.nix",
        "{ root = import ../.; bare = import dep/b.nix; }\n",
    ),
    ("nix/dep/b.nix", "{ b = 1; }\n"),
];

const R_FIXTURES: [(&str, &str); 4] = [
    (
        "R/main.R",
        "library(dplyr)\nsource(\"helpers.R\")\nsource(\"R/shared.R\")\nsource(\"missing.R\")\nrun <- function() {\n  helper()\n  dplyr::filter(df)\n}\n",
    ),
    ("helpers.R", "helper <- function() 1\n"),
    ("R/helpers.R", "sibling_helper <- function() 1\n"),
    ("R/shared.R", "shared <- function() 1\n"),
];

#[test]
fn ruby_relative_loads_resolve_and_load_path_features_are_never_guessed() {
    let forward = deterministic_generation(&RUBY_FIXTURES);
    assert_import_targets_file(&forward, "app/main.rb", "./lib/helper", "app/lib/helper.rb");
    for feature in [
        "lib/helper",
        "config/environment",
        "json",
        "missing/feature",
    ] {
        assert_unresolved_import(&forward, "app/main.rb", feature);
    }
    let run = capability_symbol(&forward, "app/main.rb", "run");
    for name in ["shared_tool", "json"] {
        let target = capability_symbol(&forward, "app/tools.rb", name);
        let call = CapabilityReferenceQuery::new(&forward, run).named(name, ReferenceKind::Calls);
        assert_eq!(
            call.target_symbol_id.as_ref(),
            Some(&target.symbol_id),
            "a gem require must not suppress project resolution of {name}"
        );
    }
}

#[test]
fn lua_requires_follow_package_path_templates_from_the_project_root() {
    let forward = deterministic_generation(&LUA_FIXTURES);
    assert_import_targets_file(&forward, "main.lua", "app.utils", "app/utils.lua");
    assert_import_targets_file(&forward, "main.lua", "app.net", "app/net/init.lua");
    assert_import_targets_file(&forward, "main.lua", "lib/codec", "lib/codec.lua");
    for missing in ["json", "app.missing", "./lib/codec"] {
        assert_unresolved_import(&forward, "main.lua", missing);
    }
    let start = capability_symbol(&forward, "main.lua", "start");
    let helper = capability_symbol(&forward, "lib/globals.lua", "helper_global");
    let call =
        CapabilityReferenceQuery::new(&forward, start).named("helper_global", ReferenceKind::Calls);
    assert_eq!(call.target_symbol_id.as_ref(), Some(&helper.symbol_id));
    let member =
        CapabilityReferenceQuery::new(&forward, start).named("utils.pack", ReferenceKind::Calls);
    assert!(
        member.target_symbol_id.is_none(),
        "a require alias names the returned table, not the module's globals: {member:?}"
    );
}

#[test]
fn nix_imports_load_exact_paths_or_directory_default_files_only() {
    let forward = deterministic_generation(&NIX_FIXTURES);
    assert_import_targets_file(&forward, "default.nix", "./pkgs/a.nix", "pkgs/a.nix");
    assert_import_targets_file(&forward, "default.nix", "./lib", "lib/default.nix");
    for missing in ["./missing.nix", "./other", "<nixpkgs>"] {
        assert_unresolved_import(&forward, "default.nix", missing);
    }
    assert_import_targets_file(&forward, "nix/main.nix", "../.", "default.nix");
    assert_import_targets_file(&forward, "nix/main.nix", "./dep/b.nix", "nix/dep/b.nix");
}

#[test]
fn r_source_loads_root_relative_paths_and_packages_stay_external() {
    let forward = deterministic_generation(&R_FIXTURES);
    assert_import_targets_file(&forward, "R/main.R", "helpers.R", "helpers.R");
    assert_import_targets_file(&forward, "R/main.R", "R/shared.R", "R/shared.R");
    for missing in ["dplyr", "missing.R"] {
        assert_unresolved_import(&forward, "R/main.R", missing);
    }
    let run = capability_symbol(&forward, "R/main.R", "run");
    let helper = capability_symbol(&forward, "helpers.R", "helper");
    let call = CapabilityReferenceQuery::new(&forward, run).named("helper", ReferenceKind::Calls);
    assert_eq!(call.target_symbol_id.as_ref(), Some(&helper.symbol_id));
    let package_call =
        CapabilityReferenceQuery::new(&forward, run).named("dplyr::filter", ReferenceKind::Calls);
    assert!(package_call.target_symbol_id.is_none());
    assert_eq!(
        package_call.resolution_provenance,
        EXTERNAL_REFERENCE_UNRESOLVED_PROVENANCE
    );
}

fn deterministic_generation(fixtures: &[(&str, &str)]) -> CanonicalGenerationFacts {
    let forward = build_capability_generation(fixtures, false);
    let reversed = build_capability_generation(fixtures, true);
    assert_eq!(forward.digest(), reversed.digest());
    assert_eq!(forward.references(), reversed.references());
    assert_eq!(forward.edges(), reversed.edges());
    forward
}

fn assert_unresolved_import(facts: &CanonicalGenerationFacts, source_path: &str, module: &str) {
    let source = capability_file_symbol(facts, source_path);
    let reference =
        CapabilityReferenceQuery::new(facts, source).named(module, ReferenceKind::Imports);
    assert!(
        reference.target_symbol_id.is_none(),
        "{module} must stay unresolved: {reference:?}"
    );
    assert_eq!(
        reference.resolution_provenance,
        UNRESOLVED_IMPORT_PROVENANCE
    );
}
