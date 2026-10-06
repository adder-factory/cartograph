//! HCL / Terraform extraction parity with the v1 `HclExtractor` contract:
//! Terraform-address identities, block kinds, locals, module sources, and
//! address references.

mod credential_support;
mod dependency_ownership;

use cartograph_domain::{FileParseStatus, ReferenceKind, SourceLanguage, SymbolKind};
use cartograph_extract::{
    ExtractError, ExtractedFile, NativeExtractor, SourceLimits, SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;

fn extract(source: &str) -> ExtractedFile {
    let snapshot = SourceSnapshot::from_bytes("infra/main.tf", source.as_bytes(), limits())
        .unwrap_or_else(|error| panic!("HCL snapshot failed: {error}"));
    assert_eq!(snapshot.language(), SourceLanguage::Hcl);
    let mut extractor = NativeExtractor::new(SourceLanguage::Hcl)
        .unwrap_or_else(|error| panic!("HCL extractor failed: {error}"));
    let first = extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("HCL extraction failed: {error}"));
    let second = extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("HCL repeat failed: {error}"));
    assert_eq!(first, second, "HCL extraction was not deterministic");
    first
}

fn symbol<'a>(
    file: &'a ExtractedFile,
    qualified_name: &str,
) -> &'a cartograph_extract::ExtractedSymbol {
    let mut matches = file
        .symbols
        .iter()
        .filter(|symbol| symbol.qualified_name == qualified_name);
    let found = matches.next().unwrap_or_else(|| {
        panic!(
            "missing {qualified_name}; symbols={:?}",
            file.symbols
                .iter()
                .map(|symbol| (symbol.kind, &symbol.name, &symbol.qualified_name))
                .collect::<Vec<_>>()
        )
    });
    assert!(matches.next().is_none(), "duplicate {qualified_name}");
    found
}

fn reference_names(file: &ExtractedFile, kind: ReferenceKind) -> Vec<&str> {
    file.references
        .iter()
        .filter(|reference| reference.kind == kind)
        .map(|reference| reference.name.as_str())
        .collect()
}

fn references_from<'a>(file: &'a ExtractedFile, owner_qualified_name: &str) -> Vec<&'a str> {
    let owner = &symbol(file, owner_qualified_name).id;
    file.references
        .iter()
        .filter(|reference| reference.owner.as_ref() == Some(owner))
        .map(|reference| reference.name.as_str())
        .collect()
}

#[test]
fn typed_blocks_are_named_by_terraform_address_and_never_collide() {
    let file = extract(
        "resource \"aws_s3_bucket\" \"logs\" {\n  bucket = \"my-logs\"\n}\nresource \"aws_s3_bucket\" \"data\" {}\ndata \"aws_caller_identity\" \"current\" {}\n",
    );
    let logs = symbol(&file, "aws_s3_bucket.logs");
    assert_eq!(logs.kind, SymbolKind::Resource);
    assert_eq!(logs.name, "aws_s3_bucket.logs");
    assert_eq!((logs.span.start_line(), logs.span.end_line()), (1, 3));
    assert!(logs.export.exported);
    assert_eq!(
        symbol(&file, "aws_s3_bucket.data").kind,
        SymbolKind::Resource
    );
    let current = symbol(&file, "data.aws_caller_identity.current");
    assert_eq!(current.kind, SymbolKind::Resource);
    assert_eq!(current.name, "aws_caller_identity.current");
    assert!(
        file.symbols
            .iter()
            .all(|symbol| !symbol.qualified_name.starts_with("resource.")),
        "block keyword leaked into an identity: {:?}",
        file.symbols
    );
}

#[test]
fn named_blocks_take_terraform_kinds_and_prefixes() {
    let file = extract(
        "variable \"environment\" {\n  type = string\n}\noutput \"vpc_id\" { value = \"abc\" }\nprovider \"aws\" { region = \"us-east-1\" }\nmodule \"vpc\" { source = \"terraform-aws-modules/vpc/aws\" }\ncheck \"health\" {\n  assert { condition = true }\n}\n",
    );
    for (qualified_name, kind, name) in [
        ("var.environment", SymbolKind::Variable, "environment"),
        ("output.vpc_id", SymbolKind::Export, "vpc_id"),
        ("provider.aws", SymbolKind::Namespace, "aws"),
        ("module.vpc", SymbolKind::Module, "vpc"),
        ("check.health", SymbolKind::Namespace, "health"),
    ] {
        let found = symbol(&file, qualified_name);
        assert_eq!((found.kind, found.name.as_str()), (kind, name));
    }
    assert_eq!(
        file.symbols.len(),
        5,
        "nested blocks must not become symbols: {:?}",
        file.symbols
    );
}

#[test]
fn blocks_missing_required_labels_are_dropped() {
    let file = extract("resource \"aws_s3_bucket\" {}\nvariable {}\nlocals {}\n");
    assert!(file.symbols.is_empty(), "{:?}", file.symbols);
}

#[test]
fn locals_become_one_constant_per_attribute_at_their_own_line() {
    let file = extract(
        "locals {\n  bucket_name = \"my-bucket\"\n  retention   = 30\n  enabled     = true\n}\n",
    );
    let mut constants = file
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Constant)
        .map(|symbol| symbol.qualified_name.as_str())
        .collect::<Vec<_>>();
    constants.sort_unstable();
    assert_eq!(
        constants,
        ["local.bucket_name", "local.enabled", "local.retention"]
    );
    assert_eq!(symbol(&file, "local.retention").span.start_line(), 3);
    assert_eq!(symbol(&file, "local.retention").name, "retention");
    assert!(
        file.containments.is_empty(),
        "locals are top-level declarations"
    );
    assert!(
        !format!("{file:?}").contains("my-bucket"),
        "local values are literals and must not be retained"
    );
}

#[test]
fn terraform_settings_block_is_one_module_without_nested_symbols() {
    let file = extract(
        "terraform {\n  required_version = \">= 1.0\"\n  required_providers {\n    aws = { source = \"hashicorp/aws\" }\n  }\n}\n",
    );
    let terraform = symbol(&file, "terraform");
    assert_eq!(terraform.kind, SymbolKind::Module);
    assert_eq!(file.symbols.len(), 1, "{:?}", file.symbols);
    assert!(file.references.is_empty(), "{:?}", file.references);
}

#[test]
fn address_references_use_exact_terraform_address_shapes() {
    let file = extract(
        "resource \"aws_s3_bucket_versioning\" \"v\" {\n  bucket = aws_s3_bucket.logs.id\n  name   = var.bucket_name\n  tags   = local.common_tags\n  versioning_configuration {\n    status = var.versioning_status\n  }\n}\noutput \"vpc_id\" { value = module.vpc.vpc_id }\noutput \"account\" { value = data.aws_caller_identity.current.account_id }\noutput \"short\" { value = data.foo }\n",
    );
    assert_eq!(
        references_from(&file, "aws_s3_bucket_versioning.v"),
        [
            "aws_s3_bucket.logs",
            "var.bucket_name",
            "local.common_tags",
            "var.versioning_status"
        ]
    );
    assert_eq!(references_from(&file, "output.vpc_id"), ["module.vpc"]);
    assert_eq!(
        references_from(&file, "output.account"),
        ["data.aws_caller_identity.current"]
    );
    assert_eq!(references_from(&file, "output.short"), [] as [&str; 0]);
    assert!(
        file.references
            .iter()
            .all(|reference| reference.kind == ReferenceKind::References)
    );
    let bucket = file
        .references
        .iter()
        .find(|reference| reference.name == "var.bucket_name")
        .unwrap_or_else(|| panic!("missing var.bucket_name"));
    assert_eq!(
        (bucket.span.start_line(), bucket.span.start_column()),
        (3, 11)
    );
    assert_eq!(
        bucket.span.end_byte() - bucket.span.start_byte(),
        "var.bucket_name".len() as u64
    );
}

#[test]
fn interpolations_are_scanned_and_reserved_heads_are_not_addresses() {
    let file = extract(
        "locals { name = \"${var.environment}-${random_id.suffix.hex}\" }\nresource \"aws_instance\" \"web\" {\n  count   = 3\n  enabled = true\n  empty   = null\n  tags = {\n    Idx  = \"x-${count.index}\"\n    Val  = each.value\n    Self = self.id\n    P    = path.module\n    Ws   = terraform.workspace\n    Name = var.name\n  }\n}\n",
    );
    assert_eq!(
        references_from(&file, "local.name"),
        ["var.environment", "random_id.suffix"]
    );
    assert_eq!(references_from(&file, "aws_instance.web"), ["var.name"]);
}

#[test]
fn for_expression_and_dynamic_block_bindings_are_not_references() {
    let file = extract(
        "output \"ids\" { value = [for s in var.subnets : s.id] }\nlocals {\n  tags = { for k, v in var.input : k => \"${v}-suffix\" }\n  out  = [for item in var.maps : item.id if item.enabled]\n  mix  = [for s in var.subnets : \"${s.cidr}-${var.region}\"]\n}\nresource \"aws_instance\" \"web\" {\n  dynamic \"ebs\" {\n    for_each = var.volumes\n    content { size = ebs.value.size }\n  }\n  dynamic \"rule\" {\n    for_each = var.rules\n    iterator = r\n    content { port = r.value.port }\n  }\n}\n",
    );
    assert_eq!(references_from(&file, "output.ids"), ["var.subnets"]);
    assert_eq!(references_from(&file, "local.tags"), ["var.input"]);
    assert_eq!(references_from(&file, "local.out"), ["var.maps"]);
    assert_eq!(
        references_from(&file, "local.mix"),
        ["var.subnets", "var.region"]
    );
    assert_eq!(
        references_from(&file, "aws_instance.web"),
        ["var.volumes", "var.rules"]
    );
}

#[test]
fn dynamic_iterators_are_bound_only_inside_their_content_blocks() {
    let file = extract(
        "resource \"aws_security_group\" \"web\" {\n  dynamic \"ingress\" {\n    for_each = ingress.rules\n    content {\n      from_port = ingress.value.port\n      dynamic \"cidr\" {\n        for_each = ingress.value.cidrs\n        content { block = cidr.value }\n      }\n    }\n  }\n  egress = ingress.defaults\n}\n",
    );
    assert_eq!(
        references_from(&file, "aws_security_group.web"),
        ["ingress.rules", "ingress.defaults"],
        "for_each and attributes after the dynamic block see the enclosing scope"
    );
}

#[test]
fn module_sources_import_only_static_credential_free_strings() {
    let file = extract(
        "module \"vpc\" { source = \"terraform-aws-modules/vpc/aws\" }\nmodule \"local\" { source = \"./modules/vpc\" }\nmodule \"pinned\" { source = \"git::https://example.com/vpc.git?ref=v1.2.0\" }\nmodule \"ssh\" { source = \"git::ssh://git@example.com/vpc.git\" }\nmodule \"scp\" { source = \"git@example.com:org/vpc.git\" }\nmodule \"dynamic\" { source = \"git::${var.repo}\" }\nmodule \"directive\" { source = \"%{ if true }./a%{ endif }\" }\nmodule \"leaky\" { source = \"git::https://user:hunter2@example.com/vpc.git\" }\nmodule \"token_user\" { source = \"git::https://ghp_examplecredential1@example.com/vpc.git\" }\nmodule \"token_path\" { source = \"https://example.com/glpat-examplecredential2/vpc.zip\" }\nmodule \"query\" { source = \"https://example.com/vpc.zip?token=abc123\" }\nmodule \"ref_token\" { source = \"git::https://example.com/vpc.git?ref=ghp_examplecredential3\" }\nmodule \"stripe_path\" { source = \"https://example.com/sk_live_FAKE1234567890abcdef/module.zip\" }\nmodule \"aws_path\" { source = \"https://bucket.example/AKIAIOSFODNN7EXAMPLE/vpc.zip\" }\nmodule \"slack_path\" { source = \"https://example.com/xoxb_slackexample4/vpc.zip\" }\nmodule \"password_path\" { source = \"https://example.com/password/hunter3/vpc.zip\" }\nmodule \"token_word\" { source = \"https://example.com/token/opaque5/vpc.zip\" }\nmodule \"entropy_path\" { source = \"https://example.com/Zx9mQ2vL8kP4rT6yW1nB3cD5/vpc.zip\" }\nmodule \"local_dir\" { source = \"./modules/token\" }\nmodule \"region\" { source = \"./regions/asia-east1\" }\nmodule \"computed\" { source = local.path }\nmodule \"sourceless\" { count = 2 }\nresource \"x\" \"y\" { source = \"./not-a-module\" }\n",
    );
    assert_eq!(
        reference_names(&file, ReferenceKind::Imports),
        [
            "terraform-aws-modules/vpc/aws",
            "./modules/vpc",
            "git::https://example.com/vpc.git?ref=v1.2.0",
            "git::ssh://git@example.com/vpc.git",
            "git@example.com:org/vpc.git",
            // Ordinary URL path words and opaque path names are not credentials.
            "https://example.com/password/hunter3/vpc.zip",
            "https://example.com/token/opaque5/vpc.zip",
            "https://example.com/Zx9mQ2vL8kP4rT6yW1nB3cD5/vpc.zip",
            "./modules/token",
            // Region directories share a provider-key prefix but not its shape.
            "./regions/asia-east1"
        ]
    );
    assert_eq!(references_from(&file, "module.computed"), ["local.path"]);
    let vpc_import = file
        .references
        .iter()
        .find(|reference| reference.kind == ReferenceKind::Imports)
        .unwrap_or_else(|| panic!("missing module import"));
    assert_eq!(
        vpc_import.owner.as_ref(),
        Some(&symbol(&file, "module.vpc").id)
    );
    assert_eq!(references_from(&file, "module.dynamic"), ["var.repo"]);
    let rendered = format!("{file:?}");
    for secret in [
        "hunter2",
        "abc123",
        "examplecredential1",
        "examplecredential2",
        "examplecredential3",
        "FAKE1234567890abcdef",
        "AKIAIOSFODNN7EXAMPLE",
        "slackexample4",
    ] {
        assert!(!rendered.contains(secret), "{secret} leaked");
    }
}

#[test]
fn truncated_blocks_keep_their_declaration_and_references() {
    let file = extract("resource \"aws_s3_bucket\" \"logs\" {\n  bucket = var.x\n");
    assert_eq!(
        symbol(&file, "aws_s3_bucket.logs").kind,
        SymbolKind::Resource
    );
    assert_eq!(references_from(&file, "aws_s3_bucket.logs"), ["var.x"]);
    assert_eq!(file.parse_status, FileParseStatus::Partial);
}

#[test]
fn hcl_extraction_is_cancellation_safe() {
    let snapshot = SourceSnapshot::from_bytes(
        "infra/main.tf",
        b"variable \"region\" {}\nresource \"aws_vpc\" \"this\" { cidr = var.region }\n",
        limits(),
    )
    .unwrap_or_else(|error| panic!("HCL snapshot failed: {error}"));
    let mut extractor = NativeExtractor::new(SourceLanguage::Hcl)
        .unwrap_or_else(|error| panic!("HCL extractor failed: {error}"));
    assert_eq!(
        extractor.extract_with_cancellation(&snapshot, || true),
        Err(ExtractError::Cancelled)
    );
}

#[test]
fn unicode_labels_are_terraform_names() {
    let file = extract(
        "variable \"caf\u{e9}\" {}\noutput \"r\u{e9}sum\u{e9}\" { value = var.caf\u{e9} }\n",
    );
    assert_eq!(symbol(&file, "var.caf\u{e9}").name, "caf\u{e9}");
    assert_eq!(
        references_from(&file, "output.r\u{e9}sum\u{e9}"),
        ["var.caf\u{e9}"]
    );
    let decomposed = extract("variable \"cafe\u{301}\" {}\n");
    assert_eq!(
        symbol(&decomposed, "var.cafe\u{301}").kind,
        SymbolKind::Variable
    );
}

fn limits() -> SourceLimits {
    SourceLimits::new(SOURCE_LIMIT).unwrap_or_else(|error| panic!("HCL limit failed: {error}"))
}

#[test]
fn terraform_labels_screen_provider_keys_and_keep_token() {
    credential_support::assert_screened("main.tf", "variable \"@VALUE@\" {}\n", "token");
}
