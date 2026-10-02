use std::{
    collections::BTreeMap,
    ffi::OsString,
    fmt::{self, Write as _},
    fs::File,
    io::Read as _,
    path::PathBuf,
    time::Duration,
};

use cartograph_mcp::{ToolCall, ToolCallContext, ToolDefinition, ToolHandler as _, ToolResult};
use clap::{
    Arg, ArgAction, ArgMatches, CommandFactory as _, FromArgMatches as _, ValueHint,
    builder::{PossibleValue, PossibleValuesParser},
};
use serde_json::{Map, Number, Value};

use crate::{Cli, mcp_handler};

const LOCAL_TOOL_TIMEOUT: Duration = Duration::from_mins(10);
/// Delay between in-process admin job status polls.
const ADMIN_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(100);
const ADMIN_DEADLINE_EXCEEDED: &str = "admin job exceeded the CLI deadline";
const PROJECT_PATH_ARGUMENT: &str = "__generated_project_path";
const PROJECT_PATH_LONG: &str = "project-path";
const COMPAT_ALIAS_VALUE: &str = "__compat_alias_value";
const COMPAT_ASK_PATH: &str = "__compat_ask_path";
const COMPAT_FILES_TARGET: &str = "__compat_files_target";
const COMPAT_FILES: &str = "__compat_files";
const COMPAT_STDIN: &str = "__compat_stdin";
const COMPAT_JSON: &str = "__compat_json";
const COMPAT_QUIET: &str = "__compat_quiet";
const COMPAT_NO_CODE: &str = "__compat_no_code";
const COMPAT_NO_COMPACT: &str = "__compat_no_compact";
const COMPAT_NO_INCLUDE_TESTS: &str = "__compat_no_include_tests";
const COMPAT_NO_METADATA: &str = "__compat_no_metadata";
const COMPAT_NO_SYMBOLS: &str = "__compat_no_symbols";
const COMPAT_INCLUDE_TESTS: &str = "__compat_include_tests";
const COMPAT_POSITIVE_DEFAULT: &str = "__compat_positive_default";
const COMPAT_SECOND_POSITIONAL: &str = "__compat_second_positional";
const COMPAT_ALL: &str = "__compat_all";
const COMPAT_FORMAT: &str = "__compat_format";
const CLEAR_PARSE_CACHE_SENTINEL: &str = "__all_languages";
const MAXIMUM_COMPAT_STDIN_BYTES: u64 = 1024 * 1024;

/// Parsed static operator command or a CLI command generated from one MCP schema.
pub(super) enum ParsedCli {
    Static(Cli),
    Tool(GeneratedToolInvocation),
}

pub(super) enum ParseFailure {
    Clap(clap::Error),
    Contract(String),
}

impl fmt::Display for ParseFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Clap(error) => error.fmt(formatter),
            Self::Contract(message) => formatter.write_str(message),
        }
    }
}

impl fmt::Debug for ParseFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl From<String> for ParseFailure {
    fn from(message: String) -> Self {
        Self::Contract(message)
    }
}

/// One runtime-bound MCP tool invocation assembled by the shared schema adapter.
pub(super) struct GeneratedToolInvocation {
    tool_name: String,
    project_path: PathBuf,
    arguments: Map<String, Value>,
    render_mode: CliRenderMode,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CliRenderMode {
    #[default]
    Standard,
    Json,
    FindText,
    QuietAsk,
    QuietAffected,
    AdminProfile,
    Suppress,
}

#[cfg(test)]
impl GeneratedToolInvocation {
    pub(super) fn tool_name(&self) -> &str {
        &self.tool_name
    }

    pub(super) fn arguments(&self) -> &Map<String, Value> {
        &self.arguments
    }
}

#[derive(Clone)]
struct GeneratedSpec {
    command_name: String,
    tool_name: String,
    schema: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PropertyKind {
    String,
    Integer,
    Number,
    Boolean,
    StringArray,
    JsonArray,
    JsonObject,
}

#[derive(Clone, Copy)]
struct ArgumentConfiguration<'property> {
    property: &'property Value,
    kind: PropertyKind,
    positional: bool,
}

struct CompatibilityContext<'context> {
    spec: &'context GeneratedSpec,
    matches: &'context ArgMatches,
    project_path: &'context mut PathBuf,
    arguments: &'context mut Map<String, Value>,
}

pub(super) fn parse() -> Result<ParsedCli, ParseFailure> {
    parse_from(std::env::args_os())
}

pub(super) fn parse_from<I, T>(arguments: I) -> Result<ParsedCli, ParseFailure>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let definitions = mcp_handler::tool_definitions().map_err(|_| {
        ParseFailure::Contract("generated CLI tool contracts are invalid".to_owned())
    })?;
    let specs = generated_specs(&definitions);
    let command = command_from(&definitions, &specs)?;
    let matches = command
        .try_get_matches_from(arguments)
        .map_err(ParseFailure::Clap)?;
    if let Some((name, submatches)) = matches.subcommand()
        && let Some(spec) = specs.iter().find(|spec| spec.command_name == name)
    {
        return invocation_from_matches(spec, submatches)
            .map(ParsedCli::Tool)
            .map_err(ParseFailure::Contract);
    }
    Cli::from_arg_matches(&matches)
        .map(ParsedCli::Static)
        .map_err(ParseFailure::Clap)
}

pub(super) fn command() -> Result<clap::Command, String> {
    let definitions = mcp_handler::tool_definitions()
        .map_err(|_| "generated CLI tool contracts are invalid".to_owned())?;
    let specs = generated_specs(&definitions);
    command_from(&definitions, &specs)
}

fn command_from(
    definitions: &[ToolDefinition],
    specs: &[GeneratedSpec],
) -> Result<clap::Command, String> {
    let mut command = Cli::command();
    for spec in specs {
        let definition = definitions
            .iter()
            .find(|definition| definition.name() == spec.tool_name)
            .ok_or_else(|| "generated CLI definition disappeared".to_owned())?;
        command = command.subcommand(command_for_tool(definition, spec)?);
    }
    Ok(command)
}

fn generated_specs(definitions: &[ToolDefinition]) -> Vec<GeneratedSpec> {
    definitions
        .iter()
        .filter_map(|definition| {
            let command_name = command_name(definition.name())?;
            if command_name == "status" {
                return None;
            }
            Some(GeneratedSpec {
                command_name,
                tool_name: definition.name().to_owned(),
                schema: definition.input_schema().clone(),
            })
        })
        .collect()
}

fn command_name(tool_name: &str) -> Option<String> {
    tool_name
        .strip_prefix("cartograph_")
        .map(|name| name.replace('_', "-"))
}

fn command_for_tool(
    definition: &ToolDefinition,
    spec: &GeneratedSpec,
) -> Result<clap::Command, String> {
    let properties = schema_properties(&spec.schema)?;
    let required = required_properties(&spec.schema);
    let positional = positional_fields(&spec.command_name, properties, &required);
    let mut command = base_tool_command(definition, spec);
    for (name, property) in properties {
        if name == "projectPath" {
            continue;
        }
        command = command.arg(tool_property_argument(ToolPropertyInput {
            spec,
            name,
            property,
            positional: positional.get(name).copied(),
        })?);
    }
    add_compatibility_arguments(command, spec, properties)
}

fn base_tool_command(definition: &ToolDefinition, spec: &GeneratedSpec) -> clap::Command {
    clap::Command::new(spec.command_name.clone())
        .about(
            definition
                .description()
                .lines()
                .next()
                .unwrap_or("Cartograph tool")
                .to_owned(),
        )
        .arg(
            Arg::new(PROJECT_PATH_ARGUMENT)
                .short('p')
                .long(PROJECT_PATH_LONG)
                .default_value(".")
                .value_hint(ValueHint::DirPath)
                .help("Existing project root served by this in-process tool call."),
        )
}

#[derive(Clone, Copy)]
struct ToolPropertyInput<'a> {
    spec: &'a GeneratedSpec,
    name: &'a str,
    property: &'a Value,
    positional: Option<PositionalMode>,
}

fn tool_property_argument(input: ToolPropertyInput<'_>) -> Result<Arg, String> {
    let ToolPropertyInput {
        spec,
        name,
        property,
        positional,
    } = input;
    let kind = property_kind(property)?;
    let help = property
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("Cartograph tool argument")
        .to_owned();
    let mut argument = Arg::new(name.to_owned()).help(help);
    argument = match positional {
        Some(mode) => configure_positional_argument(argument, mode),
        None => configure_option_argument(OptionArgumentInput {
            argument,
            spec,
            name,
            property,
            kind,
        }),
    };
    argument = if spec.command_name == "admin" && name == "clearParseCache" {
        argument
            .long("clear-parse-cache")
            .num_args(0..=1)
            .default_missing_value(CLEAR_PARSE_CACHE_SENTINEL)
            .action(ArgAction::Set)
            .conflicts_with("clearParseCacheLanguage")
    } else {
        configure_argument(
            argument,
            &ArgumentConfiguration {
                property,
                kind,
                positional: positional.is_some(),
            },
        )
    };
    Ok(apply_v1_short_alias(&spec.command_name, name, argument))
}

fn configure_positional_argument(mut argument: Arg, mode: PositionalMode) -> Arg {
    argument = argument.index(mode.index);
    if mode.joined_or_variadic {
        argument = argument.num_args(1..);
    }
    if mode.required {
        argument = argument.required(true);
    }
    argument
}

struct OptionArgumentInput<'a> {
    argument: Arg,
    spec: &'a GeneratedSpec,
    name: &'a str,
    property: &'a Value,
    kind: PropertyKind,
}

fn configure_option_argument(input: OptionArgumentInput<'_>) -> Arg {
    let OptionArgumentInput {
        mut argument,
        spec,
        name,
        property,
        kind,
    } = input;
    let long = kebab_case(name);
    argument =
        if kind == PropertyKind::Boolean && property.get("default") == Some(&Value::Bool(true)) {
            argument
                .long(format!("no-{long}"))
                .action(ArgAction::SetFalse)
        } else {
            argument.long(long)
        };
    if let ("review", "baseRef") = (spec.command_name.as_str(), name) {
        argument = argument.visible_alias("ref");
    }
    if let ("find", "by") = (spec.command_name.as_str(), name) {
        argument = argument.default_value("name");
    }
    argument
}

fn apply_v1_short_alias(command: &str, name: &str, argument: Arg) -> Arg {
    match (command, name) {
        (_, "limit") => argument.short('l'),
        ("ask", "retrieveK") => argument.short('k'),
        ("ask", "system") => argument.short('s'),
        ("affected", "depth") | ("graph", "direction") => argument.short('d'),
        ("affected", "filter") | ("context", "format") => argument.short('f'),
        ("context", "maxNodes") => argument.short('n'),
        _ => apply_v1_admin_and_graph_alias(command, name, argument),
    }
}

fn apply_v1_admin_and_graph_alias(command: &str, name: &str, argument: Arg) -> Arg {
    match (command, name) {
        ("graph", "edgeKind") => argument.short('e'),
        ("graph", "k") => argument.short('k').visible_alias("top-k"),
        ("admin", "force") => argument.short('f'),
        ("admin", "index") => argument.short('i'),
        ("admin", "verbose") => argument.short('v'),
        ("admin", "concurrency") => argument.short('c'),
        ("admin", "workers") => argument.visible_alias("parse-workers"),
        ("admin", "skipProjectChecks") => argument.visible_alias("no-project-checks"),
        _ => argument,
    }
}

fn add_compatibility_arguments(
    command: clap::Command,
    spec: &GeneratedSpec,
    properties: &Map<String, Value>,
) -> Result<clap::Command, String> {
    let command = add_compatibility_positional_alias(command, spec, properties)?;
    Ok(add_command_compatibility_arguments(
        command,
        spec.command_name.as_str(),
    ))
}

const COMPATIBILITY_POSITIONAL_ALIASES: [(&str, &str, &str); 9] = [
    ("summaries", "action", "action"),
    ("note", "action", "action"),
    ("review", "mode", "mode"),
    ("find", "query", "query"),
    ("explore", "query", "query"),
    ("graph", "start", "start"),
    ("imports", "pathFilter", "file"),
    ("ask", "question", "question"),
    ("trace-to-culprits", "trace", "trace"),
];

fn add_compatibility_positional_alias(
    mut command: clap::Command,
    spec: &GeneratedSpec,
    properties: &Map<String, Value>,
) -> Result<clap::Command, String> {
    let positional_alias = COMPATIBILITY_POSITIONAL_ALIASES
        .iter()
        .find(|(candidate, ..)| *candidate == spec.command_name)
        .map(|(_, property, long)| (*property, *long));
    if let Some((property_name, long)) = positional_alias
        && let Some(property) = properties.get(property_name)
    {
        let kind = property_kind(property)?;
        let mut argument = configure_argument(
            Arg::new(COMPAT_ALIAS_VALUE)
                .long(long)
                .help(format!("Compatibility alias for {property_name}."))
                .conflicts_with(property_name),
            &ArgumentConfiguration {
                property,
                kind,
                positional: false,
            },
        );
        if spec.command_name == "explore" {
            argument = argument.visible_alias("start");
        }
        command = command.arg(argument);
    }
    Ok(command)
}

fn add_command_compatibility_arguments(
    command: clap::Command,
    command_name: &str,
) -> clap::Command {
    match command_name {
        "ask" => add_ask_compatibility_arguments(command),
        "affected" => add_affected_compatibility_arguments(command),
        "files" => add_files_compatibility_arguments(command),
        "context" => add_context_compatibility_arguments(command),
        "graph" => add_graph_compatibility_arguments(command),
        "compare-to-ref" => command.arg(positive_default_argument(
            "suppress-line-range-only",
            "suppressLineRangeOnly",
        )),
        _ => add_secondary_compatibility_arguments(command, command_name),
    }
}

fn add_secondary_compatibility_arguments(
    command: clap::Command,
    command_name: &str,
) -> clap::Command {
    match command_name {
        "find" => command.arg(
            Arg::new(COMPAT_FORMAT)
                .long("format")
                .default_value("json")
                .value_parser(PossibleValuesParser::new(["text", "json"]))
                .help("Output format. JSON remains the compatibility default."),
        ),
        "host" => command.arg(positive_default_argument(
            "include-install-targets",
            "includeInstallTargets",
        )),
        "dead-code" | "imports" => command.arg(positive_default_argument(
            "exclude-fixtures",
            "excludeFixtures",
        )),
        "coverage" => command.arg(positive_default_argument("include-tests", "includeTests")),
        "admin" => add_admin_compatibility_arguments(command),
        "review" => add_review_compatibility_arguments(command),
        "session" => add_session_compatibility_arguments(command),
        "summaries" => add_summaries_compatibility_arguments(command),
        _ => command,
    }
}

fn add_ask_compatibility_arguments(command: clap::Command) -> clap::Command {
    command
        .arg(
            Arg::new(COMPAT_ASK_PATH)
                .index(2)
                .value_hint(ValueHint::DirPath)
                .help("Compatibility project-path positional."),
        )
        .arg(
            Arg::new(COMPAT_QUIET)
                .short('q')
                .long("quiet")
                .action(ArgAction::SetTrue)
                .help("Print only the answer text."),
        )
}

fn add_affected_compatibility_arguments(command: clap::Command) -> clap::Command {
    command
        .arg(
            Arg::new(COMPAT_FILES)
                .long("files")
                .num_args(1..)
                .action(ArgAction::Append)
                .conflicts_with("files")
                .conflicts_with(COMPAT_STDIN)
                .help("Compatibility alias for positional changed files."),
        )
        .arg(
            Arg::new(COMPAT_STDIN)
                .long("stdin")
                .action(ArgAction::SetTrue)
                .conflicts_with("files")
                .help("Read one changed file per line from stdin."),
        )
        .arg(
            Arg::new(COMPAT_INCLUDE_TESTS)
                .long("include-tests")
                .action(ArgAction::SetTrue)
                .help("Accepted for v1 CLI compatibility; affected tests are always included."),
        )
        .arg(json_argument())
        .arg(quiet_argument("Print only affected test paths."))
}

fn add_files_compatibility_arguments(command: clap::Command) -> clap::Command {
    command
        .arg(
            Arg::new(COMPAT_FILES_TARGET)
                .index(1)
                .help("Directory or file target selected by --format."),
        )
        .arg(
            Arg::new(COMPAT_NO_SYMBOLS)
                .long("no-symbols")
                .action(ArgAction::SetTrue)
                .conflicts_with("symbols"),
        )
        .arg(
            Arg::new(COMPAT_NO_METADATA)
                .long("no-metadata")
                .action(ArgAction::SetTrue)
                .conflicts_with_all(["metadata", "includeMetadata"]),
        )
        .arg(json_argument())
}

fn add_context_compatibility_arguments(command: clap::Command) -> clap::Command {
    command.arg(
        Arg::new(COMPAT_NO_CODE)
            .long("no-code")
            .action(ArgAction::SetTrue)
            .conflicts_with_all(["code", "includeCode"]),
    )
}

fn add_graph_compatibility_arguments(command: clap::Command) -> clap::Command {
    command
        .arg(
            Arg::new(COMPAT_NO_COMPACT)
                .long("no-compact")
                .action(ArgAction::SetTrue)
                .conflicts_with("compact"),
        )
        .arg(
            Arg::new(COMPAT_NO_INCLUDE_TESTS)
                .long("no-include-tests")
                .action(ArgAction::SetTrue)
                .conflicts_with("includeTests"),
        )
}

fn add_admin_compatibility_arguments(command: clap::Command) -> clap::Command {
    command
        .arg(
            Arg::new(COMPAT_SECOND_POSITIONAL)
                .index(2)
                .value_hint(ValueHint::DirPath)
                .help("Compatibility project path for the selected admin action."),
        )
        .arg(
            Arg::new(COMPAT_ALL)
                .long("all")
                .action(ArgAction::SetTrue)
                .help("Summarize every eligible symbol without a pass cap."),
        )
        .arg(json_argument())
        .arg(quiet_argument("Suppress successful command output."))
}

fn add_review_compatibility_arguments(command: clap::Command) -> clap::Command {
    command.arg(
        Arg::new(COMPAT_SECOND_POSITIONAL)
            .index(2)
            .value_hint(ValueHint::FilePath)
            .conflicts_with("diff")
            .help("Compatibility unified-diff file for review context."),
    )
}

fn add_session_compatibility_arguments(command: clap::Command) -> clap::Command {
    command.arg(
        Arg::new(COMPAT_SECOND_POSITIONAL)
            .index(2)
            .conflicts_with("id")
            .help("Compatibility session id for resume or audit."),
    )
}

fn add_summaries_compatibility_arguments(command: clap::Command) -> clap::Command {
    command.arg(
        Arg::new(COMPAT_SECOND_POSITIONAL)
            .index(2)
            .value_hint(ValueHint::FilePath)
            .conflicts_with("items")
            .help("Compatibility JSON input file for summaries save."),
    )
}

fn positive_default_argument(long: &'static str, property: &'static str) -> Arg {
    Arg::new(COMPAT_POSITIVE_DEFAULT)
        .long(long)
        .action(ArgAction::SetTrue)
        .conflicts_with(property)
        .help("Explicitly retain the v1 default behavior.")
}

fn json_argument() -> Arg {
    Arg::new(COMPAT_JSON)
        .short('j')
        .long("json")
        .action(ArgAction::SetTrue)
        .help("Print structured JSON.")
}

fn quiet_argument(help: &'static str) -> Arg {
    Arg::new(COMPAT_QUIET)
        .short('q')
        .long("quiet")
        .action(ArgAction::SetTrue)
        .conflicts_with(COMPAT_JSON)
        .help(help)
}

#[derive(Clone, Copy)]
struct PositionalMode {
    index: usize,
    required: bool,
    joined_or_variadic: bool,
}

const SPECIAL_POSITIONAL_FIELDS: [(&str, &str, bool, bool); 17] = [
    ("node", "symbols", false, true),
    ("context", "task", false, false),
    ("graph", "start", false, false),
    ("explore", "query", true, true),
    ("find", "query", false, true),
    ("blame", "symbol", false, false),
    ("history", "symbol", false, false),
    ("imports", "pathFilter", false, false),
    ("tests-for", "symbol", false, false),
    ("role", "role", false, false),
    ("sql", "query", false, false),
    ("coverage", "symbol", false, false),
    ("numerical", "mode", false, false),
    ("review", "mode", false, false),
    ("ask", "question", false, false),
    ("affected", "files", false, true),
    ("trace-to-culprits", "trace", false, false),
];

fn positional_fields(
    command: &str,
    properties: &Map<String, Value>,
    required: &[String],
) -> BTreeMap<String, PositionalMode> {
    let special = SPECIAL_POSITIONAL_FIELDS
        .iter()
        .find(|(candidate, ..)| *candidate == command)
        .map(|(_, name, required, joined)| (*name, *required, *joined));
    let mut result = BTreeMap::new();
    let mut next_index = 1_usize;
    if command == "at-range" {
        for name in ["file", "startLine", "endLine"] {
            if properties.contains_key(name) {
                result.insert(
                    name.to_owned(),
                    PositionalMode {
                        index: next_index,
                        required: false,
                        joined_or_variadic: false,
                    },
                );
                next_index = next_index.saturating_add(1);
            }
        }
    } else if let Some((name, required, joined_or_variadic)) = special
        && properties.contains_key(name)
    {
        result.insert(
            name.to_owned(),
            PositionalMode {
                index: next_index,
                required,
                joined_or_variadic,
            },
        );
        next_index = next_index.saturating_add(1);
    }
    for name in required {
        if name == "projectPath" || result.contains_key(name) || (command == "find" && name == "by")
        {
            continue;
        }
        let Some(property) = properties.get(name) else {
            continue;
        };
        let kind = property_kind(property).unwrap_or(PropertyKind::JsonObject);
        if matches!(
            kind,
            PropertyKind::String | PropertyKind::Integer | PropertyKind::Number
        ) {
            result.insert(
                name.to_owned(),
                PositionalMode {
                    index: next_index,
                    required: true,
                    joined_or_variadic: false,
                },
            );
            next_index = next_index.saturating_add(1);
        }
    }
    result
}

fn configure_argument(mut argument: Arg, configuration: &ArgumentConfiguration<'_>) -> Arg {
    match configuration.kind {
        PropertyKind::Boolean => {
            argument = if configuration.property.get("default") == Some(&Value::Bool(true))
                && !configuration.positional
            {
                argument.action(ArgAction::SetFalse)
            } else {
                argument.action(ArgAction::SetTrue)
            };
        }
        PropertyKind::StringArray if configuration.positional => {
            argument = argument.num_args(1..).action(ArgAction::Append);
        }
        PropertyKind::StringArray => {
            argument = argument.num_args(1..).action(ArgAction::Append);
        }
        _ => {
            argument = argument.num_args(1).action(ArgAction::Set);
        }
    }
    if let Some(values) = configuration.property.get("enum").and_then(Value::as_array) {
        let values = values
            .iter()
            .filter_map(Value::as_str)
            .map(|value| {
                let kebab = kebab_case(value);
                let possible = PossibleValue::new(value.to_owned());
                if kebab == value {
                    possible
                } else {
                    possible.alias(kebab)
                }
            })
            .collect::<Vec<_>>();
        if !values.is_empty() {
            argument = argument.value_parser(PossibleValuesParser::new(values));
        }
    }
    argument
}

struct InvocationArgumentInput<'input> {
    spec: &'input GeneratedSpec,
    matches: &'input ArgMatches,
    required: &'input [String],
    positional: &'input BTreeMap<String, PositionalMode>,
    name: &'input str,
    property: &'input Value,
}

fn invocation_argument_supplied(input: &InvocationArgumentInput<'_>) -> bool {
    let value_source = input.matches.value_source(input.name);
    value_source.is_some()
        && (value_source != Some(clap::parser::ValueSource::DefaultValue)
            || input.required.iter().any(|name| name == input.name))
}

fn append_clear_parse_cache_argument(
    arguments: &mut Map<String, Value>,
    input: &InvocationArgumentInput<'_>,
) -> Result<bool, String> {
    if input.spec.command_name != "admin" || input.name != "clearParseCache" {
        return Ok(false);
    }
    let raw = input
        .matches
        .get_one::<String>(input.name)
        .ok_or_else(|| "--clear-parse-cache value was unavailable".to_owned())?;
    arguments.insert(input.name.to_owned(), Value::Bool(true));
    if raw != CLEAR_PARSE_CACHE_SENTINEL {
        arguments.insert(
            "clearParseCacheLanguage".to_owned(),
            Value::String(raw.clone()),
        );
    }
    Ok(true)
}

fn string_array_match_value(input: &InvocationArgumentInput<'_>) -> Option<Value> {
    input.matches.get_many::<String>(input.name).map(|values| {
        let values = values.cloned().collect::<Vec<_>>();
        if input
            .positional
            .get(input.name)
            .is_some_and(|mode| mode.joined_or_variadic)
            && input.name == "query"
        {
            Value::String(values.join(" "))
        } else {
            Value::Array(values.into_iter().map(Value::String).collect())
        }
    })
}

fn invocation_argument_value(input: &InvocationArgumentInput<'_>) -> Result<Option<Value>, String> {
    let kind = property_kind(input.property)?;
    match kind {
        PropertyKind::Boolean => Ok(input
            .matches
            .get_one::<bool>(input.name)
            .copied()
            .map(Value::Bool)),
        PropertyKind::StringArray => Ok(string_array_match_value(input)),
        PropertyKind::Integer => input
            .matches
            .get_one::<String>(input.name)
            .map(|raw| parse_integer(input.name, raw))
            .transpose(),
        PropertyKind::Number => input
            .matches
            .get_one::<String>(input.name)
            .map(|raw| parse_number(input.name, raw))
            .transpose(),
        PropertyKind::JsonArray | PropertyKind::JsonObject => input
            .matches
            .get_one::<String>(input.name)
            .map(|raw| parse_json(input.name, raw, kind))
            .transpose(),
        PropertyKind::String => Ok(input
            .matches
            .get_one::<String>(input.name)
            .map(|raw| Value::String(canonical_string_value(input.property, raw)))),
    }
}

fn append_invocation_argument(
    arguments: &mut Map<String, Value>,
    input: &InvocationArgumentInput<'_>,
) -> Result<(), String> {
    if !invocation_argument_supplied(input) {
        return Ok(());
    }
    if append_clear_parse_cache_argument(arguments, input)? {
        return Ok(());
    }
    if let Some(value) = invocation_argument_value(input)? {
        arguments.insert(input.name.to_owned(), value);
    }
    Ok(())
}

fn invocation_from_matches(
    spec: &GeneratedSpec,
    matches: &ArgMatches,
) -> Result<GeneratedToolInvocation, String> {
    let properties = schema_properties(&spec.schema)?;
    let required = required_properties(&spec.schema);
    let positional = positional_fields(&spec.command_name, properties, &required);
    let mut project_path = matches
        .get_one::<String>(PROJECT_PATH_ARGUMENT)
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    let mut arguments = Map::new();
    for (name, property) in properties {
        if name == "projectPath" {
            continue;
        }
        append_invocation_argument(
            &mut arguments,
            &InvocationArgumentInput {
                spec,
                matches,
                required: &required,
                positional: &positional,
                name,
                property,
            },
        )?;
    }
    let render_mode = apply_compatibility_matches(CompatibilityContext {
        spec,
        matches,
        project_path: &mut project_path,
        arguments: &mut arguments,
    })?;
    Ok(GeneratedToolInvocation {
        tool_name: spec.tool_name.clone(),
        project_path,
        arguments,
        render_mode,
    })
}

fn apply_compatibility_matches(
    mut context: CompatibilityContext<'_>,
) -> Result<CliRenderMode, String> {
    apply_positional_compatibility(&mut context)?;
    apply_input_compatibility(context.spec, context.matches, context.arguments)?;
    apply_boolean_compatibility(context.spec, context.matches, context.arguments)?;
    apply_family_compatibility(&mut context)?;
    Ok(compatibility_render_mode(
        context.spec,
        context.matches,
        context.arguments,
    ))
}

fn apply_positional_compatibility(context: &mut CompatibilityContext<'_>) -> Result<(), String> {
    let alias_property = match context.spec.command_name.as_str() {
        "summaries" | "note" => Some("action"),
        "review" => Some("mode"),
        "find" | "explore" => Some("query"),
        "graph" => Some("start"),
        "imports" => Some("pathFilter"),
        "ask" => Some("question"),
        "trace-to-culprits" => Some("trace"),
        _ => None,
    };
    if let Some(property) = alias_property
        && let Some(value) = context.matches.get_one::<String>(COMPAT_ALIAS_VALUE)
    {
        let schema = schema_properties(&context.spec.schema)?
            .get(property)
            .ok_or_else(|| "compatibility alias schema disappeared".to_owned())?;
        context.arguments.insert(
            property.to_owned(),
            Value::String(canonical_string_value(schema, value)),
        );
    }
    if let Some(path) = compatibility_one(context.matches, COMPAT_ASK_PATH) {
        *context.project_path = PathBuf::from(path);
    }
    apply_files_target(context.matches, context.arguments)?;
    Ok(())
}

fn apply_files_target(
    matches: &ArgMatches,
    arguments: &mut Map<String, Value>,
) -> Result<(), String> {
    if let Some(target) = compatibility_one(matches, COMPAT_FILES_TARGET) {
        let format = arguments
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("tree");
        let field = match format {
            "symbols" | "deps" | "read" => "file",
            "module" => "dirPath",
            _ => "dir",
        };
        if arguments.contains_key(field) {
            return Err(format!(
                "the files target positional conflicts with --{}",
                kebab_case(field)
            ));
        }
        arguments.insert(field.to_owned(), Value::String(target.clone()));
    }
    Ok(())
}

fn apply_input_compatibility(
    spec: &GeneratedSpec,
    matches: &ArgMatches,
    arguments: &mut Map<String, Value>,
) -> Result<(), String> {
    if let Some(files) = matches.try_get_many::<String>(COMPAT_FILES).ok().flatten() {
        arguments.insert(
            "files".to_owned(),
            Value::Array(files.cloned().map(Value::String).collect()),
        );
    }
    if compatibility_flag(matches, COMPAT_STDIN) {
        arguments.insert("files".to_owned(), read_stdin_paths()?);
    }
    if spec.command_name == "trace-to-culprits" && !arguments.contains_key("trace") {
        arguments.insert("trace".to_owned(), Value::String(read_bounded_stdin()?));
    }
    if spec.command_name == "ask"
        && arguments.get("mode").and_then(Value::as_str) == Some("local_chat")
        && !arguments.contains_key("prompt")
        && let Some(question) = arguments.remove("question")
    {
        arguments.insert("prompt".to_owned(), question);
    }
    Ok(())
}

fn apply_boolean_compatibility(
    spec: &GeneratedSpec,
    matches: &ArgMatches,
    arguments: &mut Map<String, Value>,
) -> Result<(), String> {
    const NEGATED_FLAGS: [(&str, &str); 5] = [
        (COMPAT_NO_CODE, "code"),
        (COMPAT_NO_COMPACT, "compact"),
        (COMPAT_NO_INCLUDE_TESTS, "includeTests"),
        (COMPAT_NO_METADATA, "includeMetadata"),
        (COMPAT_NO_SYMBOLS, "symbols"),
    ];

    for (flag, property) in NEGATED_FLAGS {
        if compatibility_flag(matches, flag) {
            arguments.insert(property.to_owned(), Value::Bool(false));
        }
    }
    if compatibility_flag(matches, COMPAT_POSITIVE_DEFAULT) {
        let property = match spec.command_name.as_str() {
            "compare-to-ref" => "suppressLineRangeOnly",
            "host" => "includeInstallTargets",
            "dead-code" | "imports" => "excludeFixtures",
            "coverage" => "includeTests",
            _ => return Err("internal compatibility flag routing failed".to_owned()),
        };
        arguments.insert(property.to_owned(), Value::Bool(true));
    }
    Ok(())
}

fn compatibility_render_mode(
    spec: &GeneratedSpec,
    matches: &ArgMatches,
    arguments: &Map<String, Value>,
) -> CliRenderMode {
    if spec.command_name == "find"
        && matches.get_one::<String>(COMPAT_FORMAT).map(String::as_str) == Some("text")
    {
        CliRenderMode::FindText
    } else if compatibility_flag(matches, COMPAT_JSON) {
        CliRenderMode::Json
    } else if compatibility_flag(matches, COMPAT_QUIET)
        && spec.command_name == "admin"
        && arguments.get("profile") == Some(&Value::Bool(true))
    {
        CliRenderMode::AdminProfile
    } else if compatibility_flag(matches, COMPAT_QUIET) && spec.command_name == "ask" {
        CliRenderMode::QuietAsk
    } else if compatibility_flag(matches, COMPAT_QUIET) && spec.command_name == "affected" {
        CliRenderMode::QuietAffected
    } else if compatibility_flag(matches, COMPAT_QUIET) && spec.command_name == "admin" {
        CliRenderMode::Suppress
    } else {
        CliRenderMode::Standard
    }
}

fn canonical_string_value(property: &Value, raw: &str) -> String {
    property
        .get("enum")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .find(|candidate| *candidate == raw || kebab_case(candidate) == raw)
        .unwrap_or(raw)
        .to_owned()
}

fn apply_family_compatibility(context: &mut CompatibilityContext<'_>) -> Result<(), String> {
    let second = compatibility_one(context.matches, COMPAT_SECOND_POSITIONAL).cloned();
    match context.spec.command_name.as_str() {
        "admin" => apply_admin_compatibility(context, second.as_ref()),
        "review" => apply_review_compatibility(second.as_ref(), context.arguments),
        "session" => apply_session_compatibility(second.as_ref(), context.arguments),
        "summaries" => apply_summaries_compatibility(second.as_ref(), context.arguments),
        _ => Ok(()),
    }
}

fn apply_admin_compatibility(
    context: &mut CompatibilityContext<'_>,
    second: Option<&String>,
) -> Result<(), String> {
    let action = context
        .arguments
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| "admin action is required".to_owned())?
        .to_owned();
    if let Some(path) = second {
        if matches!(action.as_str(), "init" | "uninit") {
            if context.arguments.contains_key("path") {
                return Err("admin project positional conflicts with --path".to_owned());
            }
            context.arguments.insert(
                "path".to_owned(),
                Value::String(absolute_cli_path(path)?.to_string_lossy().into_owned()),
            );
        } else {
            *context.project_path = PathBuf::from(path);
        }
    }
    if matches!(action.as_str(), "init" | "uninit")
        && let Some(path) = context.arguments.get("path").and_then(Value::as_str)
        && !PathBuf::from(path).is_absolute()
    {
        context.arguments.insert(
            "path".to_owned(),
            Value::String(absolute_cli_path(path)?.to_string_lossy().into_owned()),
        );
    }
    if compatibility_flag(context.matches, COMPAT_ALL) {
        if action != "summarize" {
            return Err("--all is supported only by admin summarize".to_owned());
        }
        if context.arguments.contains_key("limit")
            || context.arguments.contains_key("summarizeLimit")
        {
            return Err("admin summarize accepts --all or --limit, not both".to_owned());
        }
        context
            .arguments
            .insert("summarizeLimit".to_owned(), Value::from(-1));
    }
    Ok(())
}

fn absolute_cli_path(path: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        return Ok(path);
    }
    std::env::current_dir()
        .map(|current| current.join(path))
        .map_err(|_| "could not resolve the current directory".to_owned())
}

fn apply_review_compatibility(
    second: Option<&String>,
    arguments: &mut Map<String, Value>,
) -> Result<(), String> {
    if arguments.get("mode").and_then(Value::as_str) != Some("context") {
        if second.is_some() {
            return Err("the review second positional is supported only by context".to_owned());
        }
        return Ok(());
    }
    let raw = second.cloned().or_else(|| {
        arguments
            .get("diff")
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    if let Some(raw) = raw {
        arguments.insert("diff".to_owned(), Value::String(read_diff_input(&raw)?));
    }
    Ok(())
}

fn apply_session_compatibility(
    second: Option<&String>,
    arguments: &mut Map<String, Value>,
) -> Result<(), String> {
    let Some(id) = second else {
        return Ok(());
    };
    if !matches!(
        arguments.get("action").and_then(Value::as_str),
        Some("resume" | "audit")
    ) {
        return Err("the session id positional is supported only by resume or audit".to_owned());
    }
    arguments.insert("id".to_owned(), Value::String(id.clone()));
    Ok(())
}

fn apply_summaries_compatibility(
    second: Option<&String>,
    arguments: &mut Map<String, Value>,
) -> Result<(), String> {
    match arguments.get("action").and_then(Value::as_str) {
        Some("pending") => {
            if second.is_some() {
                return Err("summaries pending does not accept a JSON input file".to_owned());
            }
            arguments
                .entry("modelHint".to_owned())
                .or_insert_with(|| Value::String("agent-cli".to_owned()));
        }
        Some("save") => {
            if !arguments.contains_key("items") {
                let raw = match second {
                    Some(path) => read_bounded_file(path, "summaries JSON")?,
                    None => read_bounded_stdin()?,
                };
                let parsed: Value = serde_json::from_str(&raw)
                    .map_err(|_| "could not parse summaries JSON".to_owned())?;
                let items = match parsed {
                    Value::Array(items) => Value::Array(items),
                    Value::Object(mut object) => object
                        .remove("items")
                        .filter(Value::is_array)
                        .ok_or_else(|| {
                            "expected a JSON array or an object with an items array".to_owned()
                        })?,
                    _ => {
                        return Err(
                            "expected a JSON array or an object with an items array".to_owned()
                        );
                    }
                };
                arguments.insert("items".to_owned(), items);
            }
            arguments
                .entry("model".to_owned())
                .or_insert_with(|| Value::String("agent-cli".to_owned()));
        }
        _ => {}
    }
    Ok(())
}

fn read_diff_input(raw: &str) -> Result<String, String> {
    if raw == "-" {
        return read_bounded_stdin();
    }
    if raw.contains('\n') || raw.starts_with("@@") || raw.starts_with("diff --git") {
        if raw.is_empty() || raw.len() as u64 > MAXIMUM_COMPAT_STDIN_BYTES || raw.contains('\0') {
            return Err("unified diff must contain 1 byte through 1 MiB".to_owned());
        }
        return Ok(raw.to_owned());
    }
    read_bounded_file(raw, "unified diff")
}

fn read_bounded_file(path: &str, label: &str) -> Result<String, String> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|_| format!("could not open {label} file"))?
        .take(MAXIMUM_COMPAT_STDIN_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| format!("could not read {label} file"))?;
    if bytes.is_empty() || bytes.len() as u64 > MAXIMUM_COMPAT_STDIN_BYTES {
        return Err(format!("{label} file must contain 1 byte through 1 MiB"));
    }
    let text = String::from_utf8(bytes).map_err(|_| format!("{label} file must be UTF-8"))?;
    if text.contains('\0') {
        return Err(format!("{label} file must not contain NUL bytes"));
    }
    Ok(text)
}

fn compatibility_one<'matches>(
    matches: &'matches ArgMatches,
    id: &str,
) -> Option<&'matches String> {
    matches.try_get_one::<String>(id).ok().flatten()
}

fn compatibility_flag(matches: &ArgMatches, id: &str) -> bool {
    matches
        .try_get_one::<bool>(id)
        .ok()
        .flatten()
        .copied()
        .unwrap_or(false)
}

fn read_bounded_stdin() -> Result<String, String> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(MAXIMUM_COMPAT_STDIN_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| "could not read stdin".to_owned())?;
    if bytes.is_empty() || bytes.len() as u64 > MAXIMUM_COMPAT_STDIN_BYTES {
        return Err("stdin must contain 1 byte through 1 MiB".to_owned());
    }
    let text = String::from_utf8(bytes).map_err(|_| "stdin must be UTF-8".to_owned())?;
    if text.contains('\0') {
        return Err("stdin must not contain NUL bytes".to_owned());
    }
    Ok(text)
}

fn read_stdin_paths() -> Result<Value, String> {
    let text = read_bounded_stdin()?;
    let paths = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(201)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if paths.is_empty() || paths.len() > 200 {
        return Err("stdin must contain between 1 and 200 non-empty paths".to_owned());
    }
    Ok(Value::Array(paths.into_iter().map(Value::String).collect()))
}

fn parse_integer(name: &str, raw: &str) -> Result<Value, String> {
    raw.parse::<i64>()
        .map(Number::from)
        .map(Value::Number)
        .map_err(|_| format!("--{} must be an integer", kebab_case(name)))
}

fn parse_number(name: &str, raw: &str) -> Result<Value, String> {
    let number = raw
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
        .and_then(Number::from_f64)
        .ok_or_else(|| format!("--{} must be a finite number", kebab_case(name)))?;
    Ok(Value::Number(number))
}

fn parse_json(name: &str, raw: &str, expected: PropertyKind) -> Result<Value, String> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|_| format!("--{} must be valid JSON", kebab_case(name)))?;
    let valid = matches!(
        (expected, &value),
        (PropertyKind::JsonArray, Value::Array(_)) | (PropertyKind::JsonObject, Value::Object(_))
    );
    if !valid {
        return Err(format!(
            "--{} has the wrong JSON container type",
            kebab_case(name)
        ));
    }
    Ok(value)
}

fn schema_properties(schema: &Value) -> Result<&Map<String, Value>, String> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| "tool input schema has no object properties".to_owned())
}

fn required_properties(schema: &Value) -> Vec<String> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn property_kind(property: &Value) -> Result<PropertyKind, String> {
    let property_type = property.get("type");
    let kind = match property_type {
        Some(Value::String(value)) => value.as_str(),
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .find(|value| *value != "null")
            .unwrap_or("string"),
        _ if property.get("anyOf").is_some() => "string",
        _ => "string",
    };
    match kind {
        "string" => Ok(PropertyKind::String),
        "integer" => Ok(PropertyKind::Integer),
        "number" => Ok(PropertyKind::Number),
        "boolean" => Ok(PropertyKind::Boolean),
        "object" => Ok(PropertyKind::JsonObject),
        "array" => {
            let item_type = property
                .get("items")
                .and_then(|items| items.get("type"))
                .and_then(Value::as_str);
            if item_type == Some("string") || property.get("items").is_none() {
                Ok(PropertyKind::StringArray)
            } else {
                Ok(PropertyKind::JsonArray)
            }
        }
        _ => Err("tool input schema has an unsupported CLI property".to_owned()),
    }
}

fn kebab_case(name: &str) -> String {
    let mut output = String::with_capacity(name.len().saturating_add(8));
    for character in name.chars() {
        if character.is_ascii_uppercase() {
            if !output.is_empty() {
                output.push('-');
            }
            output.push(character.to_ascii_lowercase());
        } else if character == '_' {
            output.push('-');
        } else {
            output.push(character);
        }
    }
    output
}

pub(super) async fn run(
    invocation: GeneratedToolInvocation,
) -> Result<std::process::ExitCode, String> {
    let render_mode = invocation.render_mode;
    let result = execute(invocation).await?;
    render_tool_result(&result, render_mode);
    if result.is_error() {
        Ok(std::process::ExitCode::FAILURE)
    } else {
        Ok(std::process::ExitCode::SUCCESS)
    }
}

async fn execute(invocation: GeneratedToolInvocation) -> Result<ToolResult, String> {
    let runtime = std::sync::Arc::new(crate::open_runtime(&invocation.project_path).await?);
    let handler = mcp_handler::CartographMcpHandler::new(runtime)
        .map_err(|_| "generated CLI handler contract is invalid".to_owned())?;
    let result = call_until_terminal(&handler, invocation).await;
    // Always cancel and reap (with a bound) an admin job this process started,
    // including when the CLI deadline elapsed, so the job gets to fail its
    // staging generation and release its lease instead of being abandoned when
    // the process exits.
    handler.shutdown().await;
    result
}

async fn call_until_terminal(
    handler: &mcp_handler::CartographMcpHandler,
    invocation: GeneratedToolInvocation,
) -> Result<ToolResult, String> {
    let tool_name = invocation.tool_name;
    let is_admin = tool_name == "cartograph_admin";
    let local_timeout = generated_tool_timeout(&tool_name, &invocation.arguments);
    let result = handler
        .call(
            ToolCall {
                name: tool_name,
                arguments: invocation.arguments,
            },
            ToolCallContext::local(local_timeout),
        )
        .await
        .map_err(|error| error.wire_message().to_owned())?;
    if is_admin {
        await_admin_terminal(handler, result).await
    } else {
        Ok(result)
    }
}

fn generated_tool_timeout(tool_name: &str, arguments: &Map<String, Value>) -> Duration {
    if tool_name != "cartograph_admin"
        || arguments.get("action").and_then(Value::as_str) != Some("biomarkers-refresh")
    {
        return LOCAL_TOOL_TIMEOUT;
    }
    let statement_timeout_ms = arguments
        .get("databaseQueryTimeoutMs")
        .or_else(|| arguments.get("timeoutMs"))
        .and_then(Value::as_u64)
        .unwrap_or(mcp_handler::ADMIN_BIOMARKER_REFRESH_DEFAULT_TIMEOUT_MS)
        .min(mcp_handler::ADMIN_BIOMARKER_REFRESH_MAXIMUM_TIMEOUT_MS);
    Duration::from_millis(statement_timeout_ms)
        .checked_add(Duration::from_mins(1))
        .unwrap_or(LOCAL_TOOL_TIMEOUT)
        .max(LOCAL_TOOL_TIMEOUT)
}

pub(super) async fn run_direct(
    tool_name: &str,
    project_path: PathBuf,
    arguments: Map<String, Value>,
) -> Result<std::process::ExitCode, String> {
    if !mcp_handler::tool_definitions()
        .map_err(|_| "generated CLI tool contracts are invalid".to_owned())?
        .iter()
        .any(|definition| definition.name() == tool_name)
    {
        return Err("requested local tool is not registered".to_owned());
    }
    run(GeneratedToolInvocation {
        tool_name: tool_name.to_owned(),
        project_path,
        arguments,
        render_mode: CliRenderMode::Standard,
    })
    .await
}

pub(super) async fn run_direct_result(
    tool_name: &str,
    project_path: PathBuf,
    arguments: Map<String, Value>,
) -> Result<ToolResult, String> {
    if !mcp_handler::tool_definitions()
        .map_err(|_| "generated CLI tool contracts are invalid".to_owned())?
        .iter()
        .any(|definition| definition.name() == tool_name)
    {
        return Err("requested local tool is not registered".to_owned());
    }
    execute(GeneratedToolInvocation {
        tool_name: tool_name.to_owned(),
        project_path,
        arguments,
        render_mode: CliRenderMode::Standard,
    })
    .await
}

async fn await_admin_terminal(
    handler: &mcp_handler::CartographMcpHandler,
    initial: ToolResult,
) -> Result<ToolResult, String> {
    let Some((job_id, status)) = admin_job_identity(&initial) else {
        return Ok(initial);
    };
    if status != "running" && status != "cancelling" {
        return Ok(initial);
    }
    let deadline = tokio::time::Instant::now() + LOCAL_TOOL_TIMEOUT;
    poll_admin_terminal(deadline, || admin_job_status(handler, job_id)).await
}

async fn admin_job_status(
    handler: &mcp_handler::CartographMcpHandler,
    job_id: u64,
) -> Result<ToolResult, String> {
    handler
        .call(
            ToolCall {
                name: "cartograph_admin".to_owned(),
                arguments: Map::from_iter([
                    ("action".to_owned(), Value::String("status".to_owned())),
                    ("jobId".to_owned(), Value::Number(Number::from(job_id))),
                ]),
            },
            ToolCallContext::local(LOCAL_TOOL_TIMEOUT),
        )
        .await
        .map_err(|error| error.wire_message().to_owned())
}

/// Poll an admin job until it leaves the running states.
///
/// The absolute deadline bounds every status request as well as the loop, so a
/// status call that never completes cannot keep the CLI waiting past it.
async fn poll_admin_terminal<Poll, PollFuture>(
    deadline: tokio::time::Instant,
    mut poll: Poll,
) -> Result<ToolResult, String>
where
    Poll: FnMut() -> PollFuture,
    PollFuture: Future<Output = Result<ToolResult, String>>,
{
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(ADMIN_DEADLINE_EXCEEDED.to_owned());
        }
        tokio::time::sleep(ADMIN_STATUS_POLL_INTERVAL).await;
        let result = tokio::time::timeout_at(deadline, poll())
            .await
            .map_err(|_| ADMIN_DEADLINE_EXCEEDED.to_owned())??;
        let Some((_, status)) = admin_job_identity(&result) else {
            return Err("admin status response was malformed".to_owned());
        };
        if !matches!(status.as_str(), "running" | "cancelling") {
            return Ok(result);
        }
    }
}

fn admin_job_identity(result: &ToolResult) -> Option<(u64, String)> {
    let value: Value = serde_json::from_str(result.primary_text()?).ok()?;
    Some((
        value.get("jobId")?.as_u64()?,
        value.get("status")?.as_str()?.to_owned(),
    ))
}

fn render_tool_result(result: &ToolResult, mode: CliRenderMode) {
    if !result.is_error() {
        match mode {
            CliRenderMode::Json if render_structured_json(result) => return,
            CliRenderMode::FindText if render_find_text(result) => return,
            CliRenderMode::QuietAsk if render_quiet_ask(result) => return,
            CliRenderMode::QuietAffected if render_quiet_affected(result) => return,
            CliRenderMode::AdminProfile if render_admin_profile(result) => return,
            CliRenderMode::Suppress => return,
            _ => {}
        }
    }
    if let Some(text) = result.primary_text() {
        print!("{text}");
        if !text.ends_with('\n') {
            println!();
        }
    }
}

fn render_find_text(result: &ToolResult) -> bool {
    let Some(structured) = result.structured_content() else {
        return false;
    };
    let Some(rendered) = render_find_text_content(structured) else {
        return false;
    };
    println!("{rendered}");
    true
}

fn render_find_text_content(structured: &Map<String, Value>) -> Option<String> {
    let root = Value::Object(structured.clone());
    let freshness = root
        .get("freshness")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let evidence = root.get("evidence")?;
    let rows = find_text_rows(evidence);
    let total = find_text_total(evidence).unwrap_or(rows.len());
    let mut output = format!("Cartograph find ({freshness})\n");
    if rows.is_empty() {
        output.push_str("No matches.");
        if let Some(reason) = evidence
            .get("abstention")
            .or_else(|| evidence.get("state"))
            .and_then(Value::as_str)
        {
            let _ = write!(output, " Reason: {reason}.");
        }
        return Some(output);
    }
    let _ = write!(
        output,
        "{total} result{}",
        if total == 1 { "" } else { "s" }
    );
    if find_text_truncated(evidence) {
        output.push_str(" (truncated)");
    }
    output.push('\n');
    for row in rows {
        output.push_str("- ");
        output.push_str(&find_text_row(&row));
        output.push('\n');
    }
    while output.ends_with('\n') {
        output.pop();
    }
    Some(output)
}

fn find_text_rows(evidence: &Value) -> Vec<Value> {
    if let Some(rows) = evidence.as_array() {
        return rows.clone();
    }
    for pointer in ["/rows", "/items", "/hits", "/references", "/report/hits"] {
        if let Some(rows) = evidence.pointer(pointer).and_then(Value::as_array) {
            return rows.clone();
        }
    }
    evidence
        .get("file")
        .filter(|value| value.is_object())
        .cloned()
        .into_iter()
        .collect()
}

fn find_text_total(evidence: &Value) -> Option<usize> {
    for pointer in [
        "/total",
        "/totalCount",
        "/report/total",
        "/report/totalMatches",
        "/report/totalMatchesInScannedFiles",
    ] {
        if let Some(total) = evidence.pointer(pointer).and_then(Value::as_u64)
            && let Ok(total) = usize::try_from(total)
        {
            return Some(total);
        }
    }
    None
}

fn find_text_truncated(evidence: &Value) -> bool {
    [
        "/truncated",
        "/report/truncated",
        "/report/resultTruncated",
        "/report/byteBudgetTruncated",
        "/report/fileInventoryTruncated",
    ]
    .into_iter()
    .any(|pointer| evidence.pointer(pointer).and_then(Value::as_bool) == Some(true))
}

fn find_text_row(row: &Value) -> String {
    let identity = row
        .get("document")
        .filter(|value| !value.is_null())
        .or_else(|| row.get("enclosingSymbol").filter(|value| !value.is_null()))
        .unwrap_or(row);
    let name = ["qualified_name", "qualifiedName", "name", "key", "relation"]
        .into_iter()
        .find_map(|key| identity.get(key).and_then(Value::as_str))
        .unwrap_or("match");
    let kind = [
        "symbol_kind",
        "symbolKind",
        "kind",
        "document_kind",
        "documentKind",
    ]
    .into_iter()
    .find_map(|key| identity.get(key).and_then(Value::as_str));
    let path = row
        .get("path")
        .or_else(|| identity.get("path"))
        .and_then(Value::as_str);
    let line = row
        .get("line")
        .or_else(|| row.get("startLine"))
        .and_then(Value::as_u64);
    let mut rendered = name.to_owned();
    if let Some(kind) = kind {
        let _ = write!(rendered, " [{kind}]");
    }
    if let Some(path) = path {
        let _ = write!(rendered, " — {path}");
        if let Some(line) = line {
            let _ = write!(rendered, ":{line}");
        }
    }
    rendered
}

fn render_admin_profile(result: &ToolResult) -> bool {
    let Some(structured) = result.structured_content() else {
        return false;
    };
    let root = Value::Object(structured.clone());
    let Some(profile) = root
        .pointer("/report/report/profile")
        .or_else(|| root.pointer("/report/profile"))
    else {
        return false;
    };
    let Ok(rendered) = serde_json::to_string(profile) else {
        return false;
    };
    println!("{rendered}");
    true
}

fn render_structured_json(result: &ToolResult) -> bool {
    let Some(structured) = result.structured_content() else {
        return false;
    };
    let Ok(rendered) = serde_json::to_string_pretty(&Value::Object(structured.clone())) else {
        return false;
    };
    println!("{rendered}");
    true
}

fn render_quiet_ask(result: &ToolResult) -> bool {
    let Some(structured) = result.structured_content() else {
        return false;
    };
    let root = Value::Object(structured.clone());
    let answer = root
        .pointer("/evidence/answer/content")
        .or_else(|| root.pointer("/completion/content"))
        .and_then(Value::as_str);
    let Some(answer) = answer else {
        return false;
    };
    println!("{answer}");
    true
}

fn render_quiet_affected(result: &ToolResult) -> bool {
    let Some(structured) = result.structured_content() else {
        return false;
    };
    let root = Value::Object(structured.clone());
    let Some(tests) = root
        .pointer("/evidence/impact/tests")
        .and_then(Value::as_array)
    else {
        return false;
    };
    for path in tests
        .iter()
        .filter_map(|test| test.get("path"))
        .filter_map(Value::as_str)
    {
        println!("{path}");
    }
    true
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;

    const ASK_ENDPOINT: &str = "http://127.0.0.1:8082";

    #[test]
    fn biomarker_refresh_cli_deadline_exceeds_the_inner_statement_timeout() {
        let default = generated_tool_timeout(
            "cartograph_admin",
            &Map::from_iter([(
                "action".to_owned(),
                Value::String("biomarkers-refresh".to_owned()),
            )]),
        );
        assert_eq!(default, LOCAL_TOOL_TIMEOUT);

        let extended = generated_tool_timeout(
            "cartograph_admin",
            &Map::from_iter([
                (
                    "action".to_owned(),
                    Value::String("biomarkers-refresh".to_owned()),
                ),
                (
                    "databaseQueryTimeoutMs".to_owned(),
                    Value::Number(Number::from(15 * 60 * 1_000_u64)),
                ),
            ]),
        );
        assert_eq!(extended, Duration::from_mins(16));
        let capped = generated_tool_timeout(
            "cartograph_admin",
            &Map::from_iter([
                (
                    "action".to_owned(),
                    Value::String("biomarkers-refresh".to_owned()),
                ),
                (
                    "databaseQueryTimeoutMs".to_owned(),
                    Value::Number(Number::from(u64::MAX)),
                ),
            ]),
        );
        assert_eq!(capped, Duration::from_mins(31));
        assert_eq!(
            generated_tool_timeout("cartograph_find", &Map::new()),
            LOCAL_TOOL_TIMEOUT
        );
    }

    #[tokio::test(start_paused = true)]
    async fn admin_wait_deadline_bounds_a_status_call_that_never_completes() {
        let deadline = tokio::time::Instant::now() + LOCAL_TOOL_TIMEOUT;
        let waited =
            poll_admin_terminal(deadline, std::future::pending::<Result<ToolResult, String>>).await;
        assert_eq!(waited, Err(ADMIN_DEADLINE_EXCEEDED.to_owned()));
        assert!(tokio::time::Instant::now() >= deadline);
    }

    #[test]
    fn every_non_status_tool_has_a_generated_public_command() {
        let definitions = mcp_handler::tool_definitions()
            .unwrap_or_else(|error| panic!("tool definitions failed: {error}"));
        let specs = generated_specs(&definitions);
        let names = specs
            .iter()
            .map(|spec| spec.command_name.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        for expected in [
            "ask",
            "blame",
            "changed-since",
            "compare-to-ref",
            "coverage",
            "dead-code",
            "deps",
            "digest",
            "explore",
            "history",
            "host",
            "hotspots",
            "imports",
            "node",
            "note",
            "playbook",
            "propose-rename",
            "role",
            "session",
            "sql",
            "summaries",
            "tests-for",
            "trace-to-culprits",
            "verify",
        ] {
            assert!(
                names.contains(expected),
                "missing generated command {expected}"
            );
        }
    }

    #[test]
    fn generated_node_and_admin_arguments_preserve_mcp_shapes() {
        let parsed = parse_from([
            "cartograph",
            "node",
            "alpha",
            "beta",
            "--include-callers",
            "--project-path",
            ".",
        ])
        .unwrap_or_else(|error| panic!("node parse failed: {error}"));
        let ParsedCli::Tool(node) = parsed else {
            panic!("node did not route through generated tool command");
        };
        assert_eq!(node.tool_name, "cartograph_node");
        assert_eq!(
            node.arguments["symbols"],
            serde_json::json!(["alpha", "beta"])
        );
        assert_eq!(node.arguments["includeCallers"], true);

        let parsed = parse_from([
            "cartograph",
            "admin",
            "sync",
            "--workers",
            "4",
            "--project-path",
            ".",
        ])
        .unwrap_or_else(|error| panic!("admin parse failed: {error}"));
        let ParsedCli::Tool(admin) = parsed else {
            panic!("admin did not route through generated tool command");
        };
        assert_eq!(admin.tool_name, "cartograph_admin");
        assert_eq!(admin.arguments["action"], "sync");
        assert_eq!(admin.arguments["workers"], 4);
    }

    #[test]
    fn nested_v1_family_forms_map_to_native_tool_arguments() {
        let parsed = parse_from([
            "cartograph",
            "admin",
            "index",
            "workspace",
            "--parse-workers",
            "3",
            "--max-file-size",
            "64kb",
            "--clear-parse-cache=typescript",
            "--profile",
            "--quiet",
        ])
        .unwrap_or_else(|error| panic!("admin compatibility parse failed: {error}"));
        let ParsedCli::Tool(admin) = parsed else {
            panic!("admin did not route through generated CLI");
        };
        assert_eq!(admin.project_path, PathBuf::from("workspace"));
        assert_eq!(admin.arguments["workers"], 3);
        assert_eq!(admin.arguments["maxFileSize"], "64kb");
        assert_eq!(admin.arguments["clearParseCache"], true);
        assert_eq!(admin.arguments["clearParseCacheLanguage"], "typescript");
        assert_eq!(admin.render_mode, CliRenderMode::AdminProfile);

        let parsed = parse_from([
            "cartograph",
            "admin",
            "summarize",
            "workspace",
            "--all",
            "--model",
            "candidate",
        ])
        .unwrap_or_else(|error| panic!("admin summarize parse failed: {error}"));
        let ParsedCli::Tool(summarize) = parsed else {
            panic!("admin summarize did not route through generated CLI");
        };
        assert_eq!(summarize.project_path, PathBuf::from("workspace"));
        assert_eq!(summarize.arguments["summarizeLimit"], -1);
        assert_eq!(summarize.arguments["model"], "candidate");

        let parsed = parse_from([
            "cartograph",
            "session",
            "resume",
            "11111111-1111-1111-1111-111111111111",
        ])
        .unwrap_or_else(|error| panic!("session resume parse failed: {error}"));
        let ParsedCli::Tool(session) = parsed else {
            panic!("session did not route through generated CLI");
        };
        assert_eq!(
            session.arguments["id"],
            "11111111-1111-1111-1111-111111111111"
        );

        let parsed = parse_from([
            "cartograph",
            "admin",
            "doctor",
            "workspace",
            "--no-project-checks",
            "--json",
        ])
        .unwrap_or_else(|error| panic!("admin doctor parse failed: {error}"));
        let ParsedCli::Tool(doctor) = parsed else {
            panic!("admin doctor did not route through generated CLI");
        };
        assert_eq!(doctor.arguments["skipProjectChecks"], true);
        assert_eq!(doctor.render_mode, CliRenderMode::Json);
    }

    #[test]
    fn review_and_summaries_file_positionals_load_bounded_inputs() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let diff = root.path().join("change.diff");
        std::fs::write(&diff, "diff --git a/a.rs b/a.rs\n@@ -1 +1 @@\n-a\n+b\n")
            .unwrap_or_else(|error| panic!("diff fixture failed: {error}"));
        let parsed = parse_from(vec![
            OsString::from("cartograph"),
            OsString::from("review"),
            OsString::from("context"),
            diff.into_os_string(),
        ])
        .unwrap_or_else(|error| panic!("review context parse failed: {error}"));
        let ParsedCli::Tool(review) = parsed else {
            panic!("review did not route through generated CLI");
        };
        assert!(
            review.arguments["diff"]
                .as_str()
                .is_some_and(|value| value.starts_with("diff --git"))
        );

        let summaries = root.path().join("summaries.json");
        std::fs::write(
            &summaries,
            r#"[{"nodeId":"symbol","contentHash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","summary":"Does work."}]"#,
        )
        .unwrap_or_else(|error| panic!("summaries fixture failed: {error}"));
        let parsed = parse_from(vec![
            OsString::from("cartograph"),
            OsString::from("summaries"),
            OsString::from("save"),
            summaries.into_os_string(),
        ])
        .unwrap_or_else(|error| panic!("summaries save parse failed: {error}"));
        let ParsedCli::Tool(save) = parsed else {
            panic!("summaries did not route through generated CLI");
        };
        assert_eq!(save.arguments["model"], "agent-cli");
        assert_eq!(save.arguments["items"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn v1_cli_aliases_preserve_values_negations_and_render_modes() {
        let parsed = parse_from(["cartograph", "find", "root", "--by", "name"])
            .unwrap_or_else(|error| panic!("find compatibility parse failed: {error}"));
        let ParsedCli::Tool(find) = parsed else {
            panic!("find did not route through generated CLI");
        };
        assert_eq!(find.arguments["by"], "name");
        assert_eq!(find.arguments["query"], "root");
        assert_eq!(find.render_mode, CliRenderMode::Standard);
        assert!(
            !find.arguments.contains_key("includeTests"),
            "branch-specific schema defaults must remain handler-owned"
        );

        let parsed = parse_from([
            "cartograph",
            "ask",
            "why",
            "workspace",
            "--model",
            "candidate",
            "--endpoint",
            ASK_ENDPOINT,
            "--quiet",
        ])
        .unwrap_or_else(|error| panic!("ask compatibility parse failed: {error}"));
        let ParsedCli::Tool(ask) = parsed else {
            panic!("ask did not route through generated CLI");
        };
        assert_eq!(ask.arguments["question"], "why");
        assert_eq!(ask.arguments["model"], "candidate");
        assert_eq!(ask.project_path, PathBuf::from("workspace"));
        assert_eq!(ask.render_mode, CliRenderMode::QuietAsk);

        let parsed = parse_from([
            "cartograph",
            "files",
            "src/lib.rs",
            "--format",
            "deps",
            "--no-symbols",
            "--json",
        ])
        .unwrap_or_else(|error| panic!("files compatibility parse failed: {error}"));
        let ParsedCli::Tool(files) = parsed else {
            panic!("files did not route through generated CLI");
        };
        assert_eq!(files.arguments["file"], "src/lib.rs");
        assert_eq!(files.arguments["symbols"], false);
        assert_eq!(files.render_mode, CliRenderMode::Json);

        let parsed = parse_from([
            "cartograph",
            "graph",
            "root",
            "--no-compact",
            "--no-include-tests",
            "--top-k",
            "7",
        ])
        .unwrap_or_else(|error| panic!("graph compatibility parse failed: {error}"));
        let ParsedCli::Tool(graph) = parsed else {
            panic!("graph did not route through generated CLI");
        };
        assert_eq!(graph.arguments["compact"], false);
        assert_eq!(graph.arguments["includeTests"], false);
        assert_eq!(graph.arguments["k"], 7);

        let parsed = parse_from([
            "cartograph",
            "imports",
            "--file",
            "src",
            "--no-exclude-fixtures",
        ])
        .unwrap_or_else(|error| panic!("imports compatibility parse failed: {error}"));
        let ParsedCli::Tool(imports) = parsed else {
            panic!("imports did not route through generated CLI");
        };
        assert_eq!(imports.arguments["pathFilter"], "src");
        assert_eq!(imports.arguments["excludeFixtures"], false);
    }

    #[test]
    fn find_format_is_independent_from_compact_and_rejects_unknown_values() {
        let parsed = parse_from([
            "cartograph",
            "find",
            "root",
            "--by",
            "name",
            "--format",
            "text",
            "--compact",
        ])
        .unwrap_or_else(|error| panic!("find text parse failed: {error}"));
        let ParsedCli::Tool(find) = parsed else {
            panic!("find did not route through generated CLI");
        };
        assert_eq!(find.render_mode, CliRenderMode::FindText);
        assert_eq!(find.arguments["compact"], true);
        assert!(!find.arguments.contains_key("format"));
        assert_matches!(
            parse_from([
                "cartograph",
                "find",
                "root",
                "--by",
                "name",
                "--format",
                "yaml",
            ])
            .err(),
            Some(ParseFailure::Clap(_))
        );
    }

    #[test]
    fn find_text_renderer_is_deterministic_and_keeps_freshness_and_truncation() {
        let structured = serde_json::json!({
            "freshness": "current",
            "evidence": {
                "rows": [
                    {
                        "name": "ProjectRuntime",
                        "kind": "struct",
                        "path": "crates/cartograph-agent/src/lib.rs",
                        "line": 793
                    }
                ],
                "total": 4,
                "truncated": true
            }
        });
        let rendered = render_find_text_content(
            structured
                .as_object()
                .unwrap_or_else(|| panic!("find text fixture was not an object")),
        )
        .unwrap_or_else(|| panic!("find text result was unavailable"));
        assert_eq!(
            rendered,
            "Cartograph find (current)\n4 results (truncated)\n- ProjectRuntime [struct] — crates/cartograph-agent/src/lib.rs:793"
        );

        let empty = serde_json::json!({
            "freshness": "stale",
            "evidence": {"rows": [], "abstention": "no_relevant_evidence"}
        });
        let rendered = render_find_text_content(
            empty
                .as_object()
                .unwrap_or_else(|| panic!("empty fixture was not an object")),
        )
        .unwrap_or_else(|| panic!("empty find text result was unavailable"));
        assert_eq!(
            rendered,
            "Cartograph find (stale)\nNo matches. Reason: no_relevant_evidence."
        );

        let module_level_reference = serde_json::json!({
            "freshness": "current",
            "evidence": {
                "references": [
                    {
                        "key": "CARTOGRAPH_TOKEN",
                        "path": "src/index.ts",
                        "line": 1,
                        "enclosingSymbol": null
                    }
                ],
                "totalExtractedReferences": 1,
                "truncated": false
            }
        });
        let rendered = render_find_text_content(
            module_level_reference
                .as_object()
                .unwrap_or_else(|| panic!("module reference fixture was not an object")),
        )
        .unwrap_or_else(|| panic!("module reference find text result was unavailable"));
        assert_eq!(
            rendered,
            "Cartograph find (current)\n1 result\n- CARTOGRAPH_TOKEN — src/index.ts:1"
        );
    }

    #[test]
    fn v1_cli_aliases_preserve_analysis_positionals_and_session_actions() {
        for (command, value, property) in [
            ("blame", "OrderService::save", "symbol"),
            ("sql", "SELECT * FROM symbols", "query"),
            ("coverage", "OrderService::save", "symbol"),
            ("numerical", "coverage", "mode"),
        ] {
            let parsed = parse_from(["cartograph", command, value]).unwrap_or_else(|error| {
                panic!("{command} positional compatibility failed: {error}")
            });
            let ParsedCli::Tool(invocation) = parsed else {
                panic!("{command} did not route through generated CLI");
            };
            assert_eq!(invocation.arguments[property], value);
        }

        for (alias, action) in [
            ("macro-save", "macro_save"),
            ("macro-run", "macro_run"),
            ("macro-list", "macro_list"),
            ("macro-delete", "macro_delete"),
        ] {
            let parsed = parse_from(["cartograph", "session", alias]).unwrap_or_else(|error| {
                panic!("session action alias {alias} failed to parse: {error}")
            });
            let ParsedCli::Tool(invocation) = parsed else {
                panic!("session action alias {alias} did not route through generated CLI");
            };
            assert_eq!(invocation.arguments["action"], action);
        }
    }

    #[test]
    fn structured_render_helpers_extract_json_answers_and_affected_paths() {
        let ask = ToolResult::text("fallback").with_structured_content(Map::from_iter([(
            "evidence".to_owned(),
            serde_json::json!({"answer": {"content": "answer only"}}),
        )]));
        assert!(render_quiet_ask(&ask));
        assert!(render_structured_json(&ask));

        let affected = ToolResult::text("fallback").with_structured_content(Map::from_iter([(
            "evidence".to_owned(),
            serde_json::json!({"impact": {"tests": [{"path": "tests/a.rs"}]}}),
        )]));
        assert!(render_quiet_affected(&affected));
    }
}
