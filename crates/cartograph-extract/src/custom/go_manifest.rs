//! Admit only a single unquoted module directive; other manifest syntax is opaque.
use super::{CustomBuilder, CustomSymbolInput, physical_lines, specifier_may_carry_credential};
use crate::{ExtractError, SourceSnapshot};
use cartograph_domain::{FileParseStatus, SourceLanguage, SymbolKind};

const MAX_MODULE_PATH_BYTES: usize = 1_024;
const MODULE_ID_PREFIX: &str = "go.module:";

pub(super) fn supports(snapshot: &SourceSnapshot) -> bool {
    snapshot.language() == SourceLanguage::Go
        && snapshot.path().as_str().rsplit('/').next() == Some("go.mod")
}

pub(super) fn extract(
    builder: &mut CustomBuilder<'_, '_>,
) -> Result<FileParseStatus, ExtractError> {
    let mut scan = ModuleScan::default();
    for (start, line) in physical_lines(builder.source()) {
        builder.check_cancelled()?;
        if !scan.line((start, line)) {
            return Ok(FileParseStatus::Partial);
        }
    }
    if scan.in_block {
        return Ok(FileParseStatus::Partial);
    }
    if let Some((path, start, end)) = scan.directive {
        builder.add_symbol(
            CustomSymbolInput::new(
                SymbolKind::Module,
                path,
                format!("{MODULE_ID_PREFIX}{path}"),
            )
            .at(start, end),
        )?;
    }
    Ok(FileParseStatus::Parsed)
}

#[derive(Default)]
struct ModuleScan<'source> {
    directive: Option<(&'source str, usize, usize)>,
    in_block: bool,
}

impl<'source> ModuleScan<'source> {
    fn line(&mut self, line: (usize, &'source str)) -> bool {
        let (start, line) = line;
        if line.contains("/*") || line.contains("*/") {
            return false;
        }
        let text = line.split_once("//").map_or(line, |(text, _)| text).trim();
        if self.in_block {
            self.in_block = text != ")";
            return true;
        }
        let mut words = text.split_whitespace();
        let command = words.next();
        let argument = words.next();
        if argument == Some("(") {
            self.in_block = matches!(
                command,
                Some("require" | "replace" | "exclude" | "retract" | "tool" | "godebug" | "ignore")
            ) && words.next().is_none();
            return self.in_block;
        }
        if command != Some("module") {
            return command != Some(")");
        }
        let Some(path) = argument.filter(|path| valid_path(path)) else {
            return false;
        };
        if words.next().is_some() || self.directive.is_some() {
            return false;
        }
        self.directive = Some((path, start, start + line.len()));
        true
    }
}

fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_MODULE_PATH_BYTES
        && !specifier_may_carry_credential(path)
        && path
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-/".contains(&byte))
        && path
            .split('/')
            .all(|segment| !segment.is_empty() && !matches!(segment, "." | ".."))
}
