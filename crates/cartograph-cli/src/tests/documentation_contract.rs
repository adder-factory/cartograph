use super::super::*;

fn marked_section<'document>(document: &'document str, start: &str, end: &str) -> &'document str {
    document
        .split_once(start)
        .and_then(|(_, remainder)| remainder.split_once(end))
        .map_or_else(
            || panic!("documentation markers `{start}` / `{end}` were missing"),
            |(section, _)| section,
        )
}

fn backtick_values(section: &str) -> std::collections::BTreeSet<String> {
    section
        .split('`')
        .enumerate()
        .filter(|(index, _)| index % 2 == 1)
        .map(|(_, value)| value.to_owned())
        .collect()
}

fn fenced_words(section: &str) -> std::collections::BTreeSet<String> {
    section
        .lines()
        .filter(|line| !line.trim_start().starts_with("```"))
        .flat_map(str::split_whitespace)
        .map(str::to_owned)
        .collect()
}

#[test]
fn public_cli_reference_tracks_commands_targets_and_affected_forms() {
    const README: &str = include_str!("../../../../README.md");
    const CLI_REFERENCE: &str = include_str!("../../../../docs/CLI-REFERENCE.md");

    let mut command = generated_cli::command()
        .unwrap_or_else(|error| panic!("generated CLI command failed: {error}"));
    command.build();
    let expected_commands = command
        .get_subcommands()
        .filter(|subcommand| !subcommand.is_hide_set() && subcommand.get_name() != "help")
        .map(|subcommand| subcommand.get_name().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    let documented_commands = backtick_values(marked_section(
        CLI_REFERENCE,
        "<!-- CARTOGRAPH_TOP_LEVEL_COMMANDS_START -->",
        "<!-- CARTOGRAPH_TOP_LEVEL_COMMANDS_END -->",
    ));
    assert_eq!(documented_commands, expected_commands);

    let expected_targets = InstallTarget::ALL
        .into_iter()
        .map(|target| target.label().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    let documented_targets = backtick_values(marked_section(
        CLI_REFERENCE,
        "<!-- CARTOGRAPH_INSTALL_TARGETS_START -->",
        "<!-- CARTOGRAPH_INSTALL_TARGETS_END -->",
    ));
    assert_eq!(documented_targets, expected_targets);

    for document in [README, CLI_REFERENCE] {
        assert!(!document.contains("cartograph affected <SYMBOL_ID>"));
        assert!(document.contains("cartograph affected --symbol-id <SYMBOL_ID>"));
    }
    assert!(
        generated_cli::parse_from(["cartograph", "affected", "src/lib.rs"]).is_ok(),
        "documented file-driven affected form did not parse"
    );
    assert!(
        generated_cli::parse_from([
            "cartograph",
            "affected",
            "--symbol-id",
            "11111111-1111-4111-8111-111111111111",
        ])
        .is_ok(),
        "documented symbol-driven affected form did not parse"
    );
}

#[test]
fn public_mcp_inventory_and_bundled_skill_track_current_contracts() {
    const MCP_USAGE: &str = include_str!("../../../../docs/MCP-USAGE.md");
    const MCP_ALIGNMENT: &str = include_str!("../../../../docs/cli-mcp-alignment.md");
    const BUNDLED_SKILL: &str = include_str!("../../assets/cartograph-skill.md");

    let expected_tools = mcp_handler::tool_definitions()
        .unwrap_or_else(|error| panic!("MCP definitions failed: {error}"))
        .into_iter()
        .map(|definition| {
            definition
                .name()
                .strip_prefix("cartograph_")
                .unwrap_or_else(|| panic!("tool name lacked Cartograph prefix"))
                .to_owned()
        })
        .collect::<std::collections::BTreeSet<_>>();
    let documented_tools = fenced_words(marked_section(
        MCP_ALIGNMENT,
        "<!-- CARTOGRAPH_MCP_TOOLS_START -->",
        "<!-- CARTOGRAPH_MCP_TOOLS_END -->",
    ));
    assert_eq!(documented_tools, expected_tools);
    assert_eq!(expected_tools.len(), 36);
    assert!(MCP_USAGE.contains("Selected high-use tools"));
    assert!(MCP_USAGE.contains("full profile advertises 36 tools"));

    for profile in McpProfile::value_variants() {
        let spelling = profile
            .to_possible_value()
            .unwrap_or_else(|| panic!("MCP profile had no CLI spelling"));
        let documented = format!("`{}`", spelling.get_name());
        assert!(
            BUNDLED_SKILL.contains(&documented),
            "bundled skill omitted MCP profile {documented}"
        );
    }
}

#[test]
fn managed_paradedb_image_pin_tracks_the_actual_extension_contract() {
    const MANAGED_IMAGE: &str = concat!(
        "paradedb/paradedb:0.26.0@sha256:",
        "52fc9c95fdfd462201168d1d82334ed61f85cbd921957cab083ec39800a217ac"
    );
    const VALIDATION_WORKFLOW: &str = include_str!("../../../../.github/workflows/v2-rust.yml");
    const DEVELOPMENT_COMPOSE: &str =
        include_str!("../../../../deploy/paradedb/docker-compose.yml");
    const README: &str = include_str!("../../../../README.md");
    const AGENT_GUIDE: &str = include_str!("../../../../AGENTS.md");
    const INSTALL_GUIDE: &str = include_str!("../../../../docs/AGENT-INSTALL.md");
    const STORAGE_GUIDE: &str = include_str!("../../../../docs/STORAGE-BACKENDS.md");
    const TROUBLESHOOTING: &str = include_str!("../../../../docs/TROUBLESHOOTING.md");

    assert_eq!(cartograph_db::MANAGED_DATABASE_IMAGE, MANAGED_IMAGE);
    assert!(VALIDATION_WORKFLOW.contains(MANAGED_IMAGE));
    assert!(DEVELOPMENT_COMPOSE.contains(MANAGED_IMAGE));
    for document in [
        README,
        AGENT_GUIDE,
        INSTALL_GUIDE,
        STORAGE_GUIDE,
        TROUBLESHOOTING,
    ] {
        assert!(document.contains("ParadeDB 0.26.0 image"));
        assert!(document.contains("`pg_search` 0.26.0"));
        assert!(document.contains("pgvector 0.8.6"));
    }
}

#[test]
fn release_docs_identify_current_audit_and_historical_benchmarks() {
    const README: &str = include_str!("../../../../README.md");
    const AGENT_INSTALL: &str = include_str!("../../../../docs/AGENT-INSTALL.md");
    const CONFIGURATION: &str = include_str!("../../../../docs/CONFIGURATION.md");
    const PERFORMANCE: &str = include_str!("../../../../docs/PERF-TUNING.md");
    const LARGE_CORPUS_BENCHMARK: &str =
        include_str!("../../../../docs/v2/benchmarks/LARGE-PUBLIC-CORPUS-STREAMING.md");
    const VERSIONED_DOCS: [&str; 7] = [
        include_str!("../../../../docs/CLI-REFERENCE.md"),
        include_str!("../../../../docs/MCP-USAGE.md"),
        include_str!("../../../../docs/SUPPORT-MATRIX.md"),
        include_str!("../../../../docs/LANGUAGE-COVERAGE-REPORT.md"),
        include_str!("../../../../docs/cli-mcp-alignment.md"),
        include_str!("../../../../docs/v2/ARCHITECTURE.md"),
        include_str!("../../../../docs/v2/GAME-SCRIPTING-LANGUAGES.md"),
    ];
    const HISTORICAL_BENCHMARKS: [&str; 3] = [
        include_str!("../../../../docs/v2/benchmarks/INDEX-SCALING.md"),
        include_str!("../../../../docs/v2/benchmarks/NATIVE-CORPUS-SCALING.md"),
        include_str!("../../../../docs/v2/benchmarks/PATCH-TASK-EVALUATION.md"),
    ];

    let release = format!("`v{}`", env!("CARGO_PKG_VERSION"));
    for document in VERSIONED_DOCS {
        assert!(
            document.contains(&release),
            "release-audited documentation omitted {release}"
        );
    }
    for document in HISTORICAL_BENCHMARKS {
        assert!(document.contains("Historical benchmark:"));
        assert!(document.contains("`pg_search` 0.25.0"));
        assert!(document.contains("pgvector 0.8.4+"));
    }

    for document in [README, AGENT_INSTALL] {
        assert!(document.contains("generationStorage"));
        assert!(document.contains("10,000 supported files"));
        assert!(document.contains("64 MiB"));
        assert!(document.contains("LARGE-PUBLIC-CORPUS-STREAMING.md"));
    }
    assert!(CONFIGURATION.contains("`generationStorage` only chooses"));
    assert!(!CONFIGURATION.contains("Storage selection is not a project-config option"));
    assert!(PERFORMANCE.contains("reported final count can therefore be intermediate"));
    assert!(LARGE_CORPUS_BENCHMARK.contains("Status: published `v2.1.11` benchmark record"));
    assert!(!LARGE_CORPUS_BENCHMARK.contains("release-candidate evidence"));
    assert!(LARGE_CORPUS_BENCHMARK.contains("not an exact tagged-binary rerun"));
}
