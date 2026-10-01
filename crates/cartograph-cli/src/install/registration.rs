//! Merge rules for rewriting an existing `cartograph` host registration.
//!
//! Cartograph owns only what it writes for a target: `command`, its own
//! server flags inside `args`, and that target's transport keys. Every other
//! key of an existing entry (`env`, `cwd`, host-specific keys) and every extra
//! server argument is preserved. An entry that launches Cartograph through a
//! wrapper (a non-Cartograph `command` whose arguments name a Cartograph
//! executable followed by `serve`) is never replaced: only that one embedded
//! argument is repinned, and only when it is an absolute path.

use std::path::Path;

use serde_json::{Map, Value};
use toml_edit::{Array, Item, TableLike, value};

use crate::host::{is_cartograph_executable, wrapped_executable_index};

/// Server arguments Cartograph writes and therefore replaces on rewrite.
const OWNED_FLAGS: [&str; 2] = ["serve", "--mcp"];
/// Server options whose value Cartograph writes and therefore replaces.
const OWNED_OPTIONS: [&str; 2] = ["--project-path", "--managed-database-port"];
/// Entry key holding the launched program.
const COMMAND_KEY: &str = "command";
/// Entry key holding the program arguments.
const ARGS_KEY: &str = "args";

/// The `command` and `args` Cartograph wants a Codex server table to hold.
pub(super) struct DesiredServer<'request> {
    pub(super) command: &'request str,
    pub(super) args: &'request [String],
}

/// Returns the JSON entry to write: `desired` merged over `existing`, or
/// `existing` with only its wrapped executable repinned to `command`.
pub(super) fn merge_json_entry(existing: Option<&Value>, desired: Value, command: &str) -> Value {
    let Some(existing) = existing.and_then(Value::as_object) else {
        return desired;
    };
    if let Some(index) = json_wrapped_index(existing) {
        return Value::Object(repin_json_wrapper(existing, index, command));
    }
    match desired {
        Value::Object(desired) => Value::Object(merge_json_direct(existing, desired)),
        other => other,
    }
}

fn json_wrapped_index(entry: &Map<String, Value>) -> Option<usize> {
    let command = entry.get(COMMAND_KEY).and_then(Value::as_str)?;
    if is_cartograph_executable(command) {
        return None;
    }
    let args = entry.get(ARGS_KEY).and_then(Value::as_array)?;
    wrapped_executable_index(args.iter().map(Value::as_str))
}

fn repin_json_wrapper(
    existing: &Map<String, Value>,
    index: usize,
    command: &str,
) -> Map<String, Value> {
    let mut entry = existing.clone();
    if let Some(argument) = entry
        .get_mut(ARGS_KEY)
        .and_then(Value::as_array_mut)
        .and_then(|args| args.get_mut(index))
        && argument.as_str().is_some_and(is_repinnable)
    {
        *argument = Value::String(command.to_owned());
    }
    entry
}

fn merge_json_direct(
    existing: &Map<String, Value>,
    desired: Map<String, Value>,
) -> Map<String, Value> {
    let mut entry = existing.clone();
    for (key, desired_value) in desired {
        let merged = if key == ARGS_KEY {
            merge_json_args(existing.get(ARGS_KEY), desired_value)
        } else {
            desired_value
        };
        entry.insert(key, merged);
    }
    entry
}

fn merge_json_args(existing: Option<&Value>, desired: Value) -> Value {
    let Value::Array(mut merged) = desired else {
        return desired;
    };
    let existing = existing
        .and_then(Value::as_array)
        .and_then(|args| args.iter().map(Value::as_str).collect::<Option<Vec<_>>>());
    if let Some(existing) = existing {
        merged.extend(
            extra_server_args(&existing)
                .into_iter()
                .map(|argument| Value::String(argument.to_owned())),
        );
    }
    Value::Array(merged)
}

/// Writes `desired` into an existing Codex server table, keeping every key
/// Cartograph does not own, or repins only the wrapped executable argument.
pub(super) fn merge_codex_server(server: &mut dyn TableLike, desired: &DesiredServer<'_>) {
    if let Some(index) = toml_wrapped_index(server) {
        repin_toml_wrapper(server, index, desired.command);
        return;
    }
    let existing_args = server
        .get(ARGS_KEY)
        .and_then(Item::as_array)
        .and_then(|args| {
            args.iter()
                .map(|argument| argument.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
        });
    let mut merged = desired.args.to_vec();
    if let Some(existing) = existing_args.as_deref() {
        let existing = existing.iter().map(String::as_str).collect::<Vec<_>>();
        merged.extend(extra_server_args(&existing).into_iter().map(str::to_owned));
    }
    if server.get(COMMAND_KEY).and_then(Item::as_str) != Some(desired.command) {
        server.insert(COMMAND_KEY, value(desired.command));
    }
    if existing_args.as_ref() != Some(&merged) {
        let mut args = Array::new();
        for argument in &merged {
            args.push(argument.as_str());
        }
        server.insert(ARGS_KEY, Item::Value(toml_edit::Value::Array(args)));
    }
}

fn toml_wrapped_index(server: &dyn TableLike) -> Option<usize> {
    let command = server.get(COMMAND_KEY).and_then(Item::as_str)?;
    if is_cartograph_executable(command) {
        return None;
    }
    let args = server.get(ARGS_KEY).and_then(Item::as_array)?;
    wrapped_executable_index(args.iter().map(toml_edit::Value::as_str))
}

fn repin_toml_wrapper(server: &mut dyn TableLike, index: usize, command: &str) {
    if let Some(args) = server.get_mut(ARGS_KEY).and_then(Item::as_array_mut)
        && args
            .get(index)
            .and_then(toml_edit::Value::as_str)
            .is_some_and(is_repinnable)
    {
        args.replace(index, command);
    }
}

/// A wrapped executable is repinned only when it is an absolute path; a
/// `PATH` lookup through the wrapper is the operator's choice and stays.
fn is_repinnable(executable: &str) -> bool {
    Path::new(executable).is_absolute()
}

/// Existing server arguments that Cartograph does not write, in order.
fn extra_server_args<'arg>(args: &[&'arg str]) -> Vec<&'arg str> {
    let mut extras = Vec::new();
    let mut arguments = args.iter().copied();
    while let Some(argument) = arguments.next() {
        if OWNED_OPTIONS.contains(&argument) {
            arguments.next();
        } else if !OWNED_FLAGS.contains(&argument) && !is_owned_inline_option(argument) {
            extras.push(argument);
        }
    }
    extras
}

fn is_owned_inline_option(argument: &str) -> bool {
    OWNED_OPTIONS.iter().any(|option| {
        argument
            .strip_prefix(option)
            .is_some_and(|rest| rest.starts_with('='))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_server_arguments_drop_only_owned_flags_and_option_values() {
        assert_eq!(
            extra_server_args(&[
                "serve",
                "--mcp",
                "--project-path",
                "/old",
                "--profile",
                "coding",
                "--managed-database-port=55433",
                "--no-auto-sync",
            ]),
            vec!["--profile", "coding", "--no-auto-sync"]
        );
    }
}
