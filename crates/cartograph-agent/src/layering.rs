use std::{collections::BTreeSet, fs, path::Path};

use globset::{Glob, GlobMatcher};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{ProjectCancellation, ProjectRuntime};
use cartograph_domain::{NormalizedPath, ProjectId};

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_LAYER_COUNT: usize = 128;
const MAX_LAYER_PATHS: usize = 256;
const MAX_POLICY_ENTRIES: usize = 256;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LayeringConfig {
    #[serde(default)]
    layers: Vec<LayerConfig>,
    #[serde(default)]
    layer_exceptions: Vec<LayerException>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LayerConfig {
    name: String,
    paths: Vec<String>,
    #[serde(default)]
    can_import: Vec<String>,
    #[serde(default)]
    cannot_import: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LayerException {
    file: String,
    can_import: Vec<String>,
}

struct CompiledLayer {
    name: String,
    paths: Vec<GlobMatcher>,
    can_import: Vec<PolicyEntry>,
    cannot_import: Vec<PolicyEntry>,
}

struct CompiledException {
    file: NormalizedPath,
    can_import: Vec<PolicyEntry>,
}

struct PolicyEntry {
    layer: Option<String>,
    path: Option<GlobMatcher>,
}

struct CompiledLayeringConfig {
    layers: Vec<CompiledLayer>,
    exceptions: Vec<CompiledException>,
    warnings: Vec<String>,
}

/// One exact current-generation import that violates the project's declared layers.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LayerViolation {
    symbol_id: String,
    path: String,
    qualified_name: String,
    finding: &'static str,
    severity: &'static str,
    start_line: u32,
    end_line: u32,
    metric_name: &'static str,
    metric: f64,
    outgoing_edges: u64,
    unresolved_references: u64,
    detail: LayerViolationDetail,
}

impl LayerViolation {
    #[must_use]
    /// Returns the symbol ID.
    pub fn symbol_id(&self) -> &str {
        &self.symbol_id
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct LayerViolationDetail {
    from_layer: String,
    to_layer: String,
    from_file: String,
    to_file: String,
    imported_specifier: String,
    confidence_basis: &'static str,
}

/// Complete bounded evaluation of opt-in project architecture policy.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LayerAnalysisReport {
    configured: bool,
    layers: usize,
    imports_evaluated: usize,
    unresolved_imports_skipped: usize,
    violations: Vec<LayerViolation>,
    configuration_warnings: Vec<String>,
}

impl LayerAnalysisReport {
    #[must_use]
    /// Returns the violations.
    pub fn violations(&self) -> &[LayerViolation] {
        &self.violations
    }

    #[must_use]
    /// Consumes this value and returns its violations.
    pub fn into_violations(self) -> Vec<LayerViolation> {
        self.violations
    }
}

/// Safe architecture-policy analysis failures.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum LayerAnalysisError {
    #[error("the Cartograph layer configuration is invalid or exceeds its bound")]
    /// Configuration violates a required provider or resource contract.
    InvalidConfiguration,
    #[error("current import evidence is unavailable")]
    /// The required durable storage operation could not complete.
    StorageUnavailable,
    #[error("layer violation source coordinates are unavailable")]
    /// Required source evidence could not be read safely.
    SourceUnavailable,
    #[error("layer analysis was cancelled")]
    /// The caller requested cancellation before the bounded operation completed.
    Cancelled,
}

impl ProjectRuntime {
    /// Evaluate exact resolved imports against opt-in `.cartograph/config.json` layer policy.
    /// # Errors
    ///
    /// Returns an error when layer configuration is malformed, unsafe, or
    /// excessive; current import or source-coordinate evidence is unavailable;
    /// the blocking evaluator fails; or cancellation wins.
    pub async fn analyze_layers(
        &self,
        project_id: &ProjectId,
        cancellation: ProjectCancellation,
    ) -> Result<LayerAnalysisReport, LayerAnalysisError> {
        if cancellation.is_cancelled() {
            return Err(LayerAnalysisError::Cancelled);
        }
        let Some(config) = read_config(&self.root)? else {
            return Ok(LayerAnalysisReport {
                configured: false,
                layers: 0,
                imports_evaluated: 0,
                unresolved_imports_skipped: 0,
                violations: Vec::new(),
                configuration_warnings: Vec::new(),
            });
        };
        let CompiledLayeringConfig {
            layers,
            exceptions,
            warnings,
        } = compile_config(config)?;
        if layers.is_empty() {
            return Ok(LayerAnalysisReport {
                configured: false,
                layers: 0,
                imports_evaluated: 0,
                unresolved_imports_skipped: 0,
                violations: Vec::new(),
                configuration_warnings: warnings,
            });
        }
        let imports = self
            .complete_static_import_hits(project_id, cancellation.clone())
            .await
            .map_err(|_| LayerAnalysisError::StorageUnavailable)?;
        tokio::task::spawn_blocking(move || {
            evaluate_imports(LayerEvaluation {
                layers: &layers,
                exceptions: &exceptions,
                imports,
                configuration_warnings: warnings,
                cancellation: &cancellation,
            })
        })
        .await
        .map_err(|_| LayerAnalysisError::SourceUnavailable)?
    }
}

fn read_config(root: &Path) -> Result<Option<LayeringConfig>, LayerAnalysisError> {
    let path = root.join(".cartograph/config.json");
    let Ok(metadata) = fs::symlink_metadata(&path) else {
        return Ok(None);
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_CONFIG_BYTES {
        return Err(LayerAnalysisError::InvalidConfiguration);
    }
    let raw = fs::read_to_string(path).map_err(|_| LayerAnalysisError::InvalidConfiguration)?;
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|_| LayerAnalysisError::InvalidConfiguration)
}

fn compile_config(config: LayeringConfig) -> Result<CompiledLayeringConfig, LayerAnalysisError> {
    if config.layers.len() > MAX_LAYER_COUNT || config.layer_exceptions.len() > MAX_LAYER_COUNT {
        return Err(LayerAnalysisError::InvalidConfiguration);
    }
    let names = config
        .layers
        .iter()
        .map(|layer| layer.name.clone())
        .collect::<BTreeSet<_>>();
    if names.len() != config.layers.len()
        || names.iter().any(|name| name.is_empty() || name.len() > 256)
    {
        return Err(LayerAnalysisError::InvalidConfiguration);
    }
    let mut warnings = Vec::new();
    let mut layers = Vec::with_capacity(config.layers.len());
    for layer in config.layers {
        if layer.paths.is_empty()
            || layer.paths.len() > MAX_LAYER_PATHS
            || layer.can_import.len() > MAX_POLICY_ENTRIES
            || layer.cannot_import.len() > MAX_POLICY_ENTRIES
        {
            return Err(LayerAnalysisError::InvalidConfiguration);
        }
        if !layer.can_import.is_empty() && !layer.cannot_import.is_empty() {
            warnings.push(format!(
                "Layer '{}' declares both canImport and cannotImport; cannotImport takes precedence",
                layer.name
            ));
        }
        layers.push(CompiledLayer {
            name: layer.name,
            paths: compile_globs(layer.paths)?,
            can_import: compile_policy(layer.can_import, &names)?,
            cannot_import: compile_policy(layer.cannot_import, &names)?,
        });
    }
    let mut exceptions = Vec::with_capacity(config.layer_exceptions.len());
    for exception in config.layer_exceptions {
        if exception.can_import.len() > MAX_POLICY_ENTRIES {
            return Err(LayerAnalysisError::InvalidConfiguration);
        }
        exceptions.push(CompiledException {
            file: NormalizedPath::parse(&exception.file)
                .map_err(|_| LayerAnalysisError::InvalidConfiguration)?,
            can_import: compile_policy(exception.can_import, &names)?,
        });
    }
    Ok(CompiledLayeringConfig {
        layers,
        exceptions,
        warnings,
    })
}

fn compile_globs(patterns: Vec<String>) -> Result<Vec<GlobMatcher>, LayerAnalysisError> {
    patterns
        .into_iter()
        .map(|pattern| {
            if pattern.is_empty() || pattern.len() > 4_096 {
                return Err(LayerAnalysisError::InvalidConfiguration);
            }
            Glob::new(&pattern)
                .map(|glob| glob.compile_matcher())
                .map_err(|_| LayerAnalysisError::InvalidConfiguration)
        })
        .collect()
}

fn compile_policy(
    entries: Vec<String>,
    names: &BTreeSet<String>,
) -> Result<Vec<PolicyEntry>, LayerAnalysisError> {
    entries
        .into_iter()
        .map(|entry| {
            if entry.is_empty() || entry.len() > 4_096 {
                return Err(LayerAnalysisError::InvalidConfiguration);
            }
            if names.contains(&entry) {
                Ok(PolicyEntry {
                    layer: Some(entry),
                    path: None,
                })
            } else {
                Glob::new(&entry)
                    .map(|glob| glob.compile_matcher())
                    .map(|path| PolicyEntry {
                        layer: None,
                        path: Some(path),
                    })
                    .map_err(|_| LayerAnalysisError::InvalidConfiguration)
            }
        })
        .collect()
}

struct LayerEvaluation<'a> {
    layers: &'a [CompiledLayer],
    exceptions: &'a [CompiledException],
    imports: Vec<crate::ImportHit>,
    configuration_warnings: Vec<String>,
    cancellation: &'a ProjectCancellation,
}

fn evaluate_imports(input: LayerEvaluation<'_>) -> Result<LayerAnalysisReport, LayerAnalysisError> {
    let LayerEvaluation {
        layers,
        exceptions,
        imports,
        configuration_warnings,
        cancellation,
    } = input;
    let import_count = imports.len();
    let mut unresolved_imports_skipped = 0_usize;
    let mut violations = Vec::new();
    for import in imports {
        if cancellation.is_cancelled() {
            return Err(LayerAnalysisError::Cancelled);
        }
        let Some(target_path) = import.target_path() else {
            unresolved_imports_skipped = unresolved_imports_skipped.saturating_add(1);
            continue;
        };
        let Some(from_layer) = matching_layer(import.file(), layers) else {
            continue;
        };
        let Some(to_layer) = matching_layer(target_path, layers) else {
            continue;
        };
        if from_layer.name == to_layer.name
            || !violates(from_layer, &to_layer.name, target_path)
            || is_excepted(
                exceptions,
                ImportRoute {
                    importer: import.file(),
                    to_layer: &to_layer.name,
                    target_path,
                },
            )
        {
            continue;
        }
        let symbol_id = import
            .symbol_id()
            .ok_or(LayerAnalysisError::StorageUnavailable)?;
        let start_line = import.line();
        violations.push(LayerViolation {
            symbol_id: symbol_id.to_owned(),
            path: import.file().to_owned(),
            qualified_name: import.specifier().to_owned(),
            finding: "illegal_import",
            severity: "warning",
            start_line,
            end_line: start_line,
            metric_name: "layer_violation",
            metric: 1.0,
            outgoing_edges: 0,
            unresolved_references: 0,
            detail: LayerViolationDetail {
                from_layer: from_layer.name.clone(),
                to_layer: to_layer.name.clone(),
                from_file: import.file().to_owned(),
                to_file: target_path.to_owned(),
                imported_specifier: import.specifier().to_owned(),
                confidence_basis: "fresh_source_resolved_current_generation_import",
            },
        });
    }
    violations.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.start_line.cmp(&right.start_line))
            .then_with(|| left.detail.to_file.cmp(&right.detail.to_file))
    });
    Ok(LayerAnalysisReport {
        configured: true,
        layers: layers.len(),
        imports_evaluated: import_count,
        unresolved_imports_skipped,
        violations,
        configuration_warnings,
    })
}

fn matching_layer<'a>(path: &str, layers: &'a [CompiledLayer]) -> Option<&'a CompiledLayer> {
    layers
        .iter()
        .find(|layer| layer.paths.iter().any(|matcher| matcher.is_match(path)))
}

fn policy_matches(entries: &[PolicyEntry], layer: &str, path: &str) -> bool {
    entries.iter().any(|entry| {
        entry.layer.as_deref() == Some(layer)
            || entry
                .path
                .as_ref()
                .is_some_and(|matcher| matcher.is_match(path))
    })
}

fn violates(from: &CompiledLayer, to_layer: &str, target_path: &str) -> bool {
    if !from.cannot_import.is_empty() {
        return policy_matches(&from.cannot_import, to_layer, target_path);
    }
    !from.can_import.is_empty() && !policy_matches(&from.can_import, to_layer, target_path)
}

fn is_excepted(exceptions: &[CompiledException], route: ImportRoute<'_>) -> bool {
    let ImportRoute {
        importer,
        to_layer,
        target_path,
    } = route;
    exceptions.iter().any(|exception| {
        exception.file.as_str() == importer
            && policy_matches(&exception.can_import, to_layer, target_path)
    })
}

#[derive(Clone, Copy)]
struct ImportRoute<'a> {
    importer: &'a str,
    to_layer: &'a str,
    target_path: &'a str,
}

#[cfg(test)]
mod tests {
    use super::{LayerConfig, LayeringConfig, compile_config, matching_layer, policy_matches};

    #[test]
    fn compiled_layers_preserve_first_match_and_layer_or_glob_policy() {
        let config = LayeringConfig {
            layers: vec![
                LayerConfig {
                    name: "ui".to_owned(),
                    paths: vec!["src/ui/**".to_owned()],
                    can_import: vec!["domain".to_owned()],
                    cannot_import: Vec::new(),
                },
                LayerConfig {
                    name: "domain".to_owned(),
                    paths: vec!["src/**".to_owned()],
                    can_import: Vec::new(),
                    cannot_import: Vec::new(),
                },
            ],
            layer_exceptions: Vec::new(),
        };
        let compiled =
            compile_config(config).unwrap_or_else(|error| panic!("config failed: {error}"));
        assert_eq!(compiled.warnings, [] as [String; 0]);
        assert_eq!(
            matching_layer("src/ui/button.ts", &compiled.layers).map(|layer| layer.name.as_str()),
            Some("ui")
        );
        assert!(policy_matches(
            &compiled.layers[0].can_import,
            "domain",
            "src/domain/user.ts"
        ));
    }
}
