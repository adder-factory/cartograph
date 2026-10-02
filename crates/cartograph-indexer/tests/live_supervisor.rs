//! Live PostgreSQL integration coverage for the bounded indexing supervisor.

#[path = "../test_support/dependency_ownership.rs"]
mod dependency_ownership;

use std::{
    env,
    future::{Future, pending, poll_fn},
    process,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    task::Poll,
    time::Duration,
};

use cartograph_config::DatabaseSettings;
use cartograph_db::{
    CanonicalGenerationFacts, CartographDatabase, CurrentGeneration, CurrentGenerationLookup,
    GenerationContents, GenerationFacts, GenerationRecoveryRequest, GenerationValidationLimits,
    LeaseOwner, LeaseRequest, LeaseTarget, NativeGenerationSpillPolicy, NewGeneration, NewProject,
    PrepareGenerationMetrics, ReadyGeneration, SearchDocumentInput, SearchQuery,
    SpilledGenerationContents, StructuralFindingQuery, StructuralFindingSeverity,
    validate_generation_facts,
};
use cartograph_domain::{
    ContentDigest, DocumentId, DocumentKind, EdgeKind, GenerationId, GenerationState, ProjectId,
    ProjectOperation,
};
use cartograph_extract::{DiscoveryLimits, SourceLimits, SourceRoot};
use cartograph_indexer::{
    CancellationReason, IndexerSupervisor, NativeGenerationBuild, NativeGenerationStorage,
    NativeParseCache, NativePipelineConfig, NativePipelineDeadlines, NativePipelineError,
    NativePipelineLimits, NativePipelineParallelism, NativePipelineReport, NativeRetainedLimits,
    PipelineFailure, PipelineFailureReason, PipelineStage, StageCapacity, StageDeadlinePolicy,
    StageEnvelope, StageExecution, StageFailureKind, StageFold, StageItemBudget, StageItemFailure,
    StageItemMeta, StageOutput, StageRunConfig, StageRunError, StageSequence, StageWorkItem,
    StageWorkload, SupervisorConfig, SupervisorError, SupervisorRequest, SupervisorState,
    build_native_generation, build_native_generation_spilled,
    build_native_generation_with_scip_and_cache,
};
use cartograph_test_support::TestSchemaGuard;
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};
use tokio::sync::oneshot;

#[path = "live_supervisor/native_corpus.rs"]
mod native_corpus;

#[path = "live_supervisor/scip_spill.rs"]
mod scip_spill;

const TEST_DATABASE_URL_ENV: &str = "CARTOGRAPH_TEST_DATABASE_URL";
const PROJECT_FINGERPRINT: &str =
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const COPY_PROBE_DOCUMENT: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const REVISION: &str = "1111111111111111111111111111111111111111";
const WORKER_COUNT: u16 = 4;
const SUCCESS_PROGRESS_STEPS: u64 = 3;
const SUCCESS_PROGRESS_BYTES: u64 = 32;
const EXPECTED_MINIMUM_HEARTBEATS: u64 = 2;
const SUCCESS_PROGRESS_DELAY: Duration = Duration::from_millis(125);
const STANDARD_OPERATION_TIMEOUT: Duration = Duration::from_secs(5);
const STANDARD_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(100);
const STANDARD_HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(500);
const STANDARD_PROGRESS_TIMEOUT: Duration = Duration::from_secs(2);
const STANDARD_CANCELLATION_GRACE: Duration = Duration::from_millis(300);
const STANDARD_COPY_TIMEOUT: Duration = Duration::from_millis(50);
const STALLED_PROGRESS_TIMEOUT: Duration = Duration::from_millis(200);
const TEST_LEASE_DURATION: Duration = Duration::from_secs(3);
// Observation waits are test-only and include executor scheduling time. Keep
// them above the LLVM-instrumented scheduling tail without changing any
// production operation, heartbeat, progress, or COPY deadline.
const LEASE_WAIT_ATTEMPTS: usize = 250;
const LEASE_WAIT_INTERVAL: Duration = Duration::from_millis(20);
const INSTRUMENTED_STAGE_WAIT_ATTEMPTS: usize = 500;
const NONCOOPERATIVE_WORK_DURATION: Duration = Duration::from_secs(2);
// Lease renewal must not depend on how the root work schedules: the blocking
// test holds a worker thread in synchronous code until it has observed 10
// heartbeat intervals, requiring only a lenient fraction of renewals. The
// release bound only ends a section the test failed to release, and stays below
// the progress timeout.
const BLOCKING_WORK_OPERATION_TIMEOUT: Duration = Duration::from_secs(10);
const BLOCKING_WORK_PROGRESS_TIMEOUT: Duration = Duration::from_secs(5);
const BLOCKING_WORK_RELEASE_BOUND: Duration = Duration::from_secs(4);
const BLOCKING_OBSERVATION_WINDOW: Duration = Duration::from_secs(1);
const EXPECTED_HEARTBEATS_WHILE_BLOCKED: u64 = 3;
// Abort cannot interrupt root work inside a synchronous section, so a cancelled
// or fenced run must wait for that section to end and keep renewing an owned
// lease until its cleanup. The settle delay outlasts the cancellation grace plus
// one more grace or heartbeat request, the bounds after which a run that gave
// up on its root would already have returned. The release bound only ends a
// section that a failed test never released.
const BLOCKED_ROOT_SETTLE: Duration = Duration::from_secs(1);
const BLOCKED_ROOT_RELEASE_BOUND: Duration = Duration::from_secs(20);
const UNREAPED_ROOT_RESULT_BOUND: Duration = Duration::from_secs(8);
// Mirrors the supervisor's database finish reserve: five heartbeat requests are
// kept after the reap ceiling for owned cleanup.
const SUPERVISOR_FINISH_DATABASE_STEPS: u32 = 5;
// The stalled configuration reaps work until its operation timeout minus the
// database finish reserve.
const STALLED_REAP_CEILING: Duration = STANDARD_OPERATION_TIMEOUT
    .saturating_sub(STANDARD_HEARTBEAT_TIMEOUT.saturating_mul(SUPERVISOR_FINISH_DATABASE_STEPS));
// Progress batches reduced inside one poll, as ordered stage reduction does,
// while concurrent status readers keep the fair progress lock contended.
const STATUS_READERS: usize = 4;
const PROGRESS_BATCH_ITEMS: usize = 8;
const PROGRESS_CONTENTION_WINDOW: Duration = Duration::from_millis(1_500);
const PROGRESS_DEADLOCK_BOUND: Duration = Duration::from_secs(10);
const STATUS_READER_JOIN_BOUND: Duration = Duration::from_secs(2);
const SHORT_CANCELLATION_GRACE: Duration = Duration::from_millis(150);
const CANCELLING_OBSERVATION_DELAY: Duration = Duration::from_millis(40);
// This test proves the whole-operation deadline despite continuous work
// progress. Keep exactly one normal heartbeat comfortably inside the active
// window so LLVM scheduling cannot turn an unrelated lease-authority deadline
// into the primary result. Dedicated tests below retain the hostile heartbeat
// timeout and uncertainty coverage.
const DEADLINE_TEST_TIMEOUT: Duration = Duration::from_secs(3);
const DEADLINE_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(800);
const DEADLINE_HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(250);
const DEADLINE_PROGRESS_TIMEOUT: Duration = Duration::from_millis(300);
const DEADLINE_CANCELLATION_GRACE: Duration = Duration::from_millis(100);
const DEADLINE_COPY_TIMEOUT: Duration = Duration::from_millis(50);
const BOUNDARY_OPERATION_TIMEOUT: Duration = Duration::from_secs(20);
const BOUNDARY_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(200);
const BOUNDARY_HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(750);
const BOUNDARY_PROGRESS_TIMEOUT: Duration = Duration::from_secs(10);
const BOUNDARY_CANCELLATION_GRACE: Duration = Duration::from_millis(500);
const BOUNDARY_COPY_TIMEOUT: Duration = Duration::from_millis(500);
const BOUNDARY_LEASE_DURATION: Duration = Duration::from_secs(6);
const UNCERTAIN_OPERATION_TIMEOUT: Duration = Duration::from_secs(4);
const UNCERTAIN_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(50);
const UNCERTAIN_HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(100);
const UNCERTAIN_PROGRESS_TIMEOUT: Duration = Duration::from_secs(2);
const UNCERTAIN_CANCELLATION_GRACE: Duration = Duration::from_secs(1);
const UNCERTAIN_COPY_TIMEOUT: Duration = Duration::from_millis(100);
const UNCERTAIN_RESULT_BOUND: Duration = Duration::from_millis(700);
const RECONCILE_OPERATION_TIMEOUT: Duration = Duration::from_secs(3);
const RECONCILE_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(50);
const RECONCILE_HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(100);
const RECONCILE_PROGRESS_TIMEOUT: Duration = Duration::from_secs(1);
const RECONCILE_CANCELLATION_GRACE: Duration = Duration::from_millis(300);
const RECONCILE_COPY_TIMEOUT: Duration = Duration::from_millis(200);
const FIRST_MUTATION_DELAY_SECONDS: &str = "0.25";
const TRANSIENT_HEARTBEAT_OPERATION_TIMEOUT: Duration = Duration::from_secs(8);
const TRANSIENT_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(100);
const TRANSIENT_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(1);
const TRANSIENT_HEARTBEAT_PROGRESS_TIMEOUT: Duration = Duration::from_secs(2);
const TRANSIENT_HEARTBEAT_LEASE_DURATION: Duration = Duration::from_secs(6);
const TRANSIENT_HEARTBEAT_DELAY_SECONDS: &str = "0.60";
const TRANSIENT_HEARTBEAT_DELAY_ATTEMPTS: i64 = 2;
// Work that finishes while a slow but successful heartbeat straddles the work
// deadline must still yield to that deadline, as the inline monitor did. The
// first heartbeat starts one interval after acquisition, 2.1 s before the 6 s
// work window closes when acquisition is instant, and a trigger holds it 2.2 s,
// below its 2.5 s statement timeout, so it always ends after the window closes;
// the work, which finishes as soon as it sees that heartbeat, then still
// finishes inside the window unless acquisition took about 2 s. The operation
// timeout is the work window plus grace, COPY, and the database finish reserve.
const HELD_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(5);
const HELD_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(3_900);
const HELD_HEARTBEAT_WORK_WINDOW: Duration = Duration::from_secs(6);
const HELD_HEARTBEAT_OPERATION_TIMEOUT: Duration = HELD_HEARTBEAT_WORK_WINDOW
    .saturating_add(STANDARD_CANCELLATION_GRACE)
    .saturating_add(STANDARD_COPY_TIMEOUT)
    .saturating_add(HELD_HEARTBEAT_TIMEOUT.saturating_mul(SUPERVISOR_FINISH_DATABASE_STEPS));
const HELD_HEARTBEAT_PROGRESS_TIMEOUT: Duration = Duration::from_millis(3_500);
const HELD_HEARTBEAT_LEASE_DURATION: Duration = Duration::from_secs(32);
const HELD_HEARTBEAT_DELAY_SECONDS: &str = "2.20";
const HELD_HEARTBEAT_DELAY_ATTEMPTS: i64 = 1;
const HELD_HEARTBEAT_POLL_INTERVAL: Duration = Duration::from_millis(50);
// The same held heartbeat must also yield to a progress stall that expires
// while it is held: the work finishes as soon as it sees the heartbeat, then
// reports no progress for the rest of the 2.2 s hold, which outlasts the 1 s
// progress timeout, while the work window stays far away.
const STALL_RACE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const STALL_RACE_PROGRESS_TIMEOUT: Duration = Duration::from_secs(1);
const STALL_RACE_WORK_WINDOW: Duration = Duration::from_secs(20);
const STALL_RACE_OPERATION_TIMEOUT: Duration = STALL_RACE_WORK_WINDOW
    .saturating_add(STANDARD_CANCELLATION_GRACE)
    .saturating_add(STANDARD_COPY_TIMEOUT)
    .saturating_add(HELD_HEARTBEAT_TIMEOUT.saturating_mul(SUPERVISOR_FINISH_DATABASE_STEPS));
const STALL_RACE_LEASE_DURATION: Duration = Duration::from_secs(30);
// Children must be reaped as soon as blocked root work is gone, not after the
// reap-time lease renewal settles. A held renewal heartbeat waits on a locked
// lease row inside its 1.5 s statement timeout while the child is observed.
const SETTLING_OPERATION_TIMEOUT: Duration = Duration::from_secs(25);
const SETTLING_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(3);
const SETTLING_LEASE_DURATION: Duration = Duration::from_secs(20);
const SETTLING_CHILD_REAP_BOUND: Duration = Duration::from_secs(1);
const EXPECTED_TRANSIENT_HEARTBEAT_ATTEMPTS: i64 = 3;
const ABORT_OPERATION_TIMEOUT: Duration = Duration::from_secs(3);
const ABORT_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(100);
const ABORT_HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(100);
// Blocked COPY and cleanup tests isolate PostgreSQL statement cancellation and
// backend reaping. Their first active heartbeat is deliberately later than the
// 100 ms fault deadline; cleanup heartbeats still have a coverage-safe but
// lease-valid request horizon. Other tests retain the hostile heartbeat values
// above so they continue to exercise heartbeat/cancellation interaction.
const ISOLATED_ABORT_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);
const ISOLATED_ABORT_HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(250);
const ABORT_PROGRESS_TIMEOUT: Duration = Duration::from_millis(400);
const BLOCKED_DATABASE_PROGRESS_TIMEOUT: Duration = Duration::from_secs(1);
const ABORT_CANCELLATION_GRACE: Duration = Duration::from_millis(100);
const ABORT_COPY_TIMEOUT: Duration = Duration::from_millis(100);
const ABORT_RESULT_BOUND: Duration = Duration::from_secs(2);
const BLOCKED_PUBLICATION_OPERATION_TIMEOUT: Duration = Duration::from_secs(2);
const BLOCKED_PUBLICATION_COPY_TIMEOUT: Duration = Duration::from_millis(500);
const BLOCKED_PUBLICATION_RESULT_BOUND: Duration = Duration::from_secs(3);
// Acquisition and test-observer setup have their own coverage-safe horizon;
// cancellation after the blocked COPY is observed remains capped separately
// by `ABORT_RESULT_BOUND`.
const COPY_CANCEL_OPERATION_TIMEOUT: Duration = Duration::from_secs(5);
const COPY_CANCEL_HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(500);
const COPY_CANCEL_GRACE: Duration = Duration::from_millis(20);
const COPY_CANCEL_NONCOOPERATIVE_TAIL: Duration = Duration::from_millis(100);
const COPY_CANCEL_TIMEOUT: Duration = Duration::from_millis(200);
const LONG_COPY_OPERATION_TIMEOUT: Duration = Duration::from_secs(3);
const LONG_COPY_TIMEOUT: Duration = Duration::from_millis(500);
const LARGE_COPY_OPERATION_TIMEOUT: Duration = Duration::from_secs(6);
const LARGE_COPY_HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(300);
const LARGE_COPY_PROGRESS_TIMEOUT: Duration = Duration::from_millis(100);
const LARGE_COPY_TIMEOUT: Duration = Duration::from_secs(1);
const LARGE_COPY_TRIGGER_DELAY_SECONDS: &str = "0.40";
const LARGE_COPY_CODE_BYTES: usize = 2 * 1_024 * 1_024;
const ORDERED_STAGE_ITEMS: u64 = 8;
const ORDERED_STAGE_WORKERS: usize = 4;
const ORDERED_STAGE_ITEM_BYTES: u64 = 16;
const NATIVE_MAX_FILES: usize = 80;
const NATIVE_MAX_PATH_BYTES: u64 = 1024 * 1024;
const NATIVE_MAX_SOURCE_BYTES: usize = 1024 * 1024;
const NATIVE_MAX_MANIFEST_BYTES: u64 = 2 * 1024 * 1024;
const NATIVE_MAX_GENERATION_BYTES: u64 = 32 * 1024 * 1024;
const NATIVE_STAGE_TIMEOUT: Duration = Duration::from_secs(3);
const SPILL_PARITY_STAGE_TIMEOUT: Duration = Duration::from_secs(30);
const SPILL_PARITY_OPERATION_TIMEOUT: Duration = Duration::from_mins(1);
const SPILL_PARITY_PROGRESS_TIMEOUT: Duration = Duration::from_secs(10);
const SPILL_PARITY_LEASE_DURATION: Duration = Duration::from_secs(30);
const CACHE_PROBE_ORIGINAL: &str = "pub fn cached_probe() -> u32 { 1 }\n";
const CACHE_PROBE_CHANGED: &str = "pub fn cached_probe() -> u32 { 2 }\n";
const SPILL_PARSE_BATCH_FILES: usize = 64;
const SPILL_ITEM_DEADLINE: Duration = Duration::from_millis(500);
const SPILL_DELAY_SECONDS: &str = "2.0";
const NATIVE_PARSER_ONLY_FILE_COUNT: usize = 6;
const NATIVE_ADMITTED_FAMILY_FILE_COUNT: usize = 18;
const NATIVE_GENERIC_FAMILY_FILE_COUNT: usize = 28;
const NATIVE_CUSTOM_FAMILY_FILE_COUNT: usize = 13;
const NATIVE_EXPECTED_FILES: u64 = 77;
const NATIVE_EXPECTED_MINIMUM_SYMBOLS: u64 = 60;
const NATIVE_EXPECTED_MINIMUM_RESOLVED_REFERENCES: u64 = 3;
const NATIVE_SEARCH_LIMIT: u16 = 10;
const NATIVE_PARSER_ONLY_FIXTURES: [(&str, &str, &str, &str); NATIVE_PARSER_ONLY_FILE_COUNT] = [
    (
        "styles/cssbeacon.css",
        "body { color: red; }",
        "css",
        "cssbeacon",
    ),
    (
        "views/templatebeacon.erb",
        "<div><%= user.name %></div>",
        "embedded_template",
        "templatebeacon",
    ),
    (
        "docs/jsdocbeacon.jsdoc",
        "/** Adds one. */\n",
        "jsdoc",
        "jsdocbeacon",
    ),
    (
        "config/jsonbeacon.json",
        r#"{"enabled":true}"#,
        "json",
        "jsonbeacon",
    ),
    (
        "notebooks/jupyterbeacon.ipynb",
        r#"{"cells":[],"metadata":{},"nbformat":4,"nbformat_minor":5}"#,
        "jupyter",
        "jupyterbeacon",
    ),
    (
        "patterns/regexbeacon.regex",
        r"[a-z]+@[a-z]+\.[a-z]+",
        "regex",
        "regexbeacon",
    ),
];
const NATIVE_ADMITTED_FAMILY_FIXTURES: [(&str, &str, &str, &str);
    NATIVE_ADMITTED_FAMILY_FILE_COUNT] = [
    (
        "native/adabeacon.adb",
        "package body AdaBeacon is\n   function AdaBeaconValue return Integer is\n   begin\n      return 1;\n   end AdaBeaconValue;\nend AdaBeacon;\n",
        "ada",
        "adabeaconvalue",
    ),
    (
        "native/cbeacon.c",
        "int cbeacon(void) { return 1; }\n",
        "c",
        "cbeacon",
    ),
    (
        "native/cppbeacon.cpp",
        "int cppbeacon() { return 1; }\n",
        "cpp",
        "cppbeacon",
    ),
    (
        "native/cudabeacon.cu",
        "__global__ void cudabeacon() {}\n",
        "cuda",
        "cudabeacon",
    ),
    (
        "native/glslbeacon.glsl",
        "void glslbeacon() {}\n",
        "glsl",
        "glslbeacon",
    ),
    (
        "native/hlslbeacon.hlsl",
        "float4 hlslbeacon() : SV_Target { return float4(1, 1, 1, 1); }\n",
        "hlsl",
        "hlslbeacon",
    ),
    (
        "native/slangbeacon.slang",
        "module beacon.shader;\n[shader(\"compute\")]\nvoid slangbeacon() {}\n",
        "slang",
        "slangbeacon",
    ),
    (
        "native/weslbeacon.wesl",
        "import package::beacon::common;\n@compute @workgroup_size(1)\nfn weslbeacon() {}\n",
        "wesl",
        "weslbeacon",
    ),
    (
        "native/bashbeacon.sh",
        "bashbeacon() { echo ok; }\n",
        "bash",
        "bashbeacon",
    ),
    (
        "native/fishbeacon.fish",
        "function fishbeacon\n  echo ok\nend\n",
        "fish",
        "fishbeacon",
    ),
    (
        "native/powershellbeacon.ps1",
        "function PowershellBeacon { Write-Output ok }\n",
        "powershell",
        "PowershellBeacon",
    ),
    (
        "native/zshbeacon.zsh",
        "zshbeacon() { print ok; }\n",
        "zsh",
        "zshbeacon",
    ),
    (
        "native/JavaBeacon.java",
        "public class JavaBeacon { public void runBeacon() {} }\n",
        "java",
        "JavaBeacon",
    ),
    (
        "native/CsharpBeacon.cs",
        "public class CsharpBeacon { public void RunBeacon() {} }\n",
        "csharp",
        "CsharpBeacon",
    ),
    (
        "native/KotlinBeacon.kt",
        "class KotlinBeacon { fun runBeacon() {} }\n",
        "kotlin",
        "KotlinBeacon",
    ),
    (
        "native/ScalaBeacon.scala",
        "class ScalaBeacon { def runBeacon(): Unit = () }\n",
        "scala",
        "ScalaBeacon",
    ),
    (
        "native/GroovyBeacon.groovy",
        "class GroovyBeacon { void runBeacon() {} }\n",
        "groovy",
        "GroovyBeacon",
    ),
    (
        "native/vhdlbeacon.vhd",
        "package vhdlbeacon is\n   function VhdlBeaconValue return integer;\nend package;\npackage body vhdlbeacon is\n   function VhdlBeaconValue return integer is\n   begin\n      return 1;\n   end function;\nend package body;\n",
        "vhdl",
        "vhdlbeaconvalue",
    ),
];
const NATIVE_GENERIC_FAMILY_FIXTURES: [(&str, &str, &str, &str); NATIVE_GENERIC_FAMILY_FILE_COUNT] = [
    (
        "generic/abapbeacon.abap",
        "CLASS zcl_beacon DEFINITION.\n PUBLIC SECTION.\n METHODS run_beacon.\nENDCLASS.\n",
        "abap",
        "zcl_beacon",
    ),
    (
        "generic/ApexBeacon.cls",
        "public class ApexBeacon { public static void runBeacon() {} }\n",
        "apex",
        "ApexBeacon",
    ),
    (
        "generic/arkbeacon.ets",
        "export function arkBeacon(): void {}\n",
        "arkts",
        "arkBeacon",
    ),
    (
        "generic/AstroBeacon.astro",
        "---\nconst AstroBeacon = 'safe';\n---\n<CustomBeacon />\n",
        "astro",
        "CustomBeacon",
    ),
    (
        "generic/clojurebeacon.clj",
        "(ns beacon.core)\n(defn clojureBeacon [] 1)\n",
        "clojure",
        "clojureBeacon",
    ),
    (
        "generic/lispbeacon.lisp",
        "(defpackage :beacon)\n(in-package :beacon)\n(defun lisp-beacon () 1)\n",
        "common_lisp",
        "lisp-beacon",
    ),
    (
        "generic/dart_beacon.dart",
        "class DartBeacon { void runBeacon() {} }\n",
        "dart",
        "DartBeacon",
    ),
    (
        "generic/FsharpBeacon.fs",
        "module FsharpBeacon\nlet fsharpBeacon value = value\n",
        "fsharp",
        "fsharpBeacon",
    ),
    (
        "generic/graphqlbeacon.graphql",
        "type GraphBeacon { beaconField: String! }\n",
        "graphql",
        "GraphBeacon",
    ),
    (
        "generic/hclbeacon.tf",
        "resource \"null_resource\" \"hcl_beacon\" { triggers = { safe = \"yes\" } }\n",
        "hcl",
        "resource",
    ),
    (
        "generic/htmlbeacon.html",
        "<custom-beacon></custom-beacon>\n",
        "html",
        "custom-beacon",
    ),
    (
        "generic/khnbeacon.khn",
        "function khnBeacon() return 1 end\n",
        "khn",
        "khnBeacon",
    ),
    (
        "generic/LeanBeacon.lean",
        "def leanBeacon : Nat := 1\n",
        "lean",
        "leanBeacon",
    ),
    (
        "generic/luabeacon.lua",
        "function luaBeacon() return 1 end\n",
        "lua",
        "luaBeacon",
    ),
    (
        "generic/luau_beacon.luau",
        "local function luauBeacon(): number return 1 end\n",
        "luau",
        "luauBeacon",
    ),
    (
        "generic/nixbeacon.nix",
        "{ nixBeacon = 1; }\n",
        "nix",
        "nixBeacon",
    ),
    (
        "generic/ObjcBeacon.m",
        "@interface ObjcBeacon : NSObject\n- (void)runBeacon;\n@end\n@implementation ObjcBeacon\n- (void)runBeacon {}\n@end\n",
        "objc",
        "ObjcBeacon",
    ),
    (
        "generic/pascalbeacon.pas",
        "program PascalBeacon;\nprocedure runBeacon; begin end;\nbegin runBeacon; end.\n",
        "pascal",
        "PascalBeacon",
    ),
    (
        "generic/PhpBeacon.php",
        "<?php class PhpBeacon { public function runBeacon() {} }\n",
        "php",
        "PhpBeacon",
    ),
    (
        "generic/prismabeacon.prisma",
        "model PrismaBeacon { id Int @id }\n",
        "prisma",
        "PrismaBeacon",
    ),
    (
        "generic/rbeacon.r",
        "rBeacon <- function(value) value\n",
        "r",
        "rBeacon",
    ),
    (
        "generic/RescriptBeacon.res",
        "let rescriptBeacon = () => ()\n",
        "rescript",
        "rescriptBeacon",
    ),
    (
        "generic/ruby_beacon.rb",
        "class RubyBeacon\n  def run_beacon\n    1\n  end\nend\n",
        "ruby",
        "RubyBeacon",
    ),
    (
        "generic/SolidityBeacon.sol",
        "contract SolidityBeacon { function runBeacon() public pure returns (uint) { return 1; } }\n",
        "solidity",
        "SolidityBeacon",
    ),
    (
        "generic/sqlbeacon.sql",
        "CREATE TABLE sql_beacon (id INTEGER PRIMARY KEY);\n",
        "sql",
        "sql_beacon",
    ),
    (
        "generic/SwiftBeacon.swift",
        "public struct SwiftBeacon { public func runBeacon() {} }\n",
        "swift",
        "SwiftBeacon",
    ),
    (
        "generic/VbBeacon.vb",
        "Public Class VbBeacon\n  Public Sub RunBeacon()\n  End Sub\nEnd Class\n",
        "vbnet",
        "VbBeacon",
    ),
    (
        "generic/yamlbeacon.yaml",
        "yamlBeacon:\n  enabled: true\n",
        "yaml",
        "yamlBeacon",
    ),
];
const NATIVE_CUSTOM_FAMILY_FIXTURES: [(&str, &str, &str, &str); NATIVE_CUSTOM_FAMILY_FILE_COUNT] = [
    (
        "force-app/main/default/aura/OrderPanel/OrderPanel.cmp",
        "<aura:component><aura:attribute name=\"auraOrderBeacon\" type=\"Id\"/></aura:component>\n",
        "aura",
        "auraOrderBeacon",
    ),
    (
        "custom/order.ann",
        "game.states.AnubisOrderBeacon = State {\n nodes.LoadOrder = Action {\n OnEnter = function()\n StartOrder()\n end\n}\n",
        "bg3_anubis",
        "AnubisOrderBeacon",
    ),
    (
        "Mods/Orders/Public/Data/order.lsx",
        "<save><node id=\"OrderDefinition\"><attribute id=\"Name\" value=\"Bg3ResourceOrderBeacon\"/></node></save>\n",
        "bg3_resource",
        "Bg3ResourceOrderBeacon",
    ),
    (
        "Game/Stats/Generated/Data/orders.txt",
        "new entry \"Bg3StatsOrderBeacon\"\nusing \"BaseOrderStats\"\n",
        "bg3_stats",
        "Bg3StatsOrderBeacon",
    ),
    (
        "sections/order-panel.liquid",
        "{% assign liquidOrderBeacon = cart.total %}\n",
        "liquid",
        "liquidOrderBeacon",
    ),
    (
        "Story/RawFiles/Goals/OrderGoal.txt",
        "INITSECTION\nsyscall OsirisOrderBeacon((GUIDSTRING)_Order)\n",
        "osiris",
        "OsirisOrderBeacon",
    ),
    (
        "config/application.properties",
        "properties.order.beacon=enabled\n",
        "properties",
        "properties.order.beacon",
    ),
    (
        "scripts/rhaibeacon.rhai",
        "fn rhaibeacon(value) { transform(value) }\n",
        "rhai",
        "rhaibeacon",
    ),
    (
        "components/SvelteOrderBeacon.svelte",
        "<script>export function svelteOrderBeacon() {}</script>\n",
        "svelte",
        "SvelteOrderBeacon",
    ),
    (
        "legacy/Vb6OrderBeacon.bas",
        "Attribute VB_Name = \"Vb6OrderBeacon\"\nPublic Sub LoadOrder()\nEnd Sub\n",
        "vb6",
        "Vb6OrderBeacon",
    ),
    (
        "force-app/main/default/pages/VisualforceOrderBeacon.page",
        "<apex:page controller=\"OrderController\"/>\n",
        "visualforce",
        "VisualforceOrderBeacon",
    ),
    (
        "components/VueOrderBeacon.vue",
        "<script setup>export function vueOrderBeacon() {}</script>\n",
        "vue",
        "VueOrderBeacon",
    ),
    (
        "mappers/XmlOrderBeacon.xml",
        "<mapper namespace=\"com.example.XmlOrderBeacon\"><select id=\"findOrder\">SELECT 1</select></mapper>\n",
        "xml",
        "XmlOrderBeacon",
    ),
];

static SCHEMA_COUNTER: AtomicU32 = AtomicU32::new(0);

struct CopyStartGate {
    ready: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

struct CopyStartControl {
    ready: oneshot::Receiver<()>,
    release: oneshot::Sender<()>,
}

fn copy_start_barrier() -> (CopyStartGate, CopyStartControl) {
    let (ready_sender, ready_receiver) = oneshot::channel();
    let (release_sender, release_receiver) = oneshot::channel();
    (
        CopyStartGate {
            ready: ready_sender,
            release: release_receiver,
        },
        CopyStartControl {
            ready: ready_receiver,
            release: release_sender,
        },
    )
}

impl CopyStartGate {
    async fn wait(self) -> Result<(), PipelineFailure> {
        self.ready
            .send(())
            .map_err(|()| PipelineFailure::new(PipelineStage::Copy))?;
        self.release
            .await
            .map_err(|_| PipelineFailure::new(PipelineStage::Copy))
    }
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn successful_supervision_renews_releases_and_requires_publication() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let target = target(&fixture.project, staged.generation_id());
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), standard_config());
    let current = supervisor
        .run(request(target.clone()), move |context| async move {
            assert!(
                context
                    .progress()
                    .begin_stage(PipelineStage::Discover)
                    .await
                    .is_ok()
            );
            for _ in 0..SUCCESS_PROGRESS_STEPS {
                tokio::time::sleep(SUCCESS_PROGRESS_DELAY).await;
                assert!(
                    context
                        .progress()
                        .advance(1, SUCCESS_PROGRESS_BYTES)
                        .await
                        .is_ok()
                );
            }
            assert!(
                context
                    .progress()
                    .begin_stage(PipelineStage::Copy)
                    .await
                    .is_ok()
            );
            context
                .prepare_generation(GenerationContents::new(
                    staged,
                    canonical(GenerationFacts::default()),
                ))
                .await
                .map_err(|_| PipelineFailure::new(PipelineStage::Copy))
        })
        .await;
    let current = match current {
        Ok(current) => current,
        Err(error) => panic!("successful supervised generation failed: {error}"),
    };
    assert_eq!(current.project_id(), &fixture.project);
    assert_eq!(
        supervisor.status().await.state(),
        SupervisorState::Completed
    );
    assert!(!supervisor.cancel());
    assert!(supervisor.status().await.heartbeat_count() >= EXPECTED_MINIMUM_HEARTBEATS);
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn transient_heartbeat_timeouts_retry_within_the_bounded_reap_horizon() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let target = target(&fixture.project, staged.generation_id());
    install_one_shot_heartbeat_delay(&fixture).await;
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), transient_heartbeat_config());
    let current = supervisor
        .run(
            request_with_duration(target.clone(), TRANSIENT_HEARTBEAT_LEASE_DURATION),
            move |context| async move {
                assert!(
                    context
                        .progress()
                        .begin_stage(PipelineStage::Discover)
                        .await
                        .is_ok()
                );
                tokio::time::sleep(Duration::from_millis(900)).await;
                assert!(context.progress().advance(1, 1).await.is_ok());
                context
                    .prepare_generation(GenerationContents::new(
                        staged,
                        canonical(GenerationFacts::default()),
                    ))
                    .await
                    .map_err(|_| PipelineFailure::new(PipelineStage::Copy))
            },
        )
        .await;
    let current = match current {
        Ok(current) => current,
        Err(error) => panic!("transient heartbeat timeouts were not retried: {error}"),
    };
    assert_eq!(current.project_id(), &fixture.project);
    assert_eq!(
        supervisor.status().await.state(),
        SupervisorState::Completed
    );
    assert!(supervisor.status().await.heartbeat_count() > 0);
    assert!(heartbeat_delay_attempts(&fixture).await >= EXPECTED_TRANSIENT_HEARTBEAT_ATTEMPTS);
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn bounded_parallel_stage_reduces_before_supervised_publication() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), standard_config());
    let current = supervisor
        .run(request(target.clone()), move |context| async move {
            let deadline = tokio::time::Instant::now() + STANDARD_OPERATION_TIMEOUT;
            let inputs = (0..ORDERED_STAGE_ITEMS).map(|sequence| {
                StageEnvelope::new(
                    StageItemMeta::new(
                        StageSequence::new(sequence),
                        format!("src/ordered_{sequence}.rs"),
                        StageItemBudget::new(
                            ORDERED_STAGE_ITEM_BYTES,
                            ORDERED_STAGE_ITEM_BYTES,
                            deadline,
                        ),
                    ),
                    sequence,
                )
            });
            let execution = StageExecution::new(
                StageRunConfig::new(
                    PipelineStage::Parse,
                    StageCapacity::new(ORDERED_STAGE_WORKERS, ORDERED_STAGE_WORKERS),
                    StageDeadlinePolicy::new(deadline, STANDARD_CANCELLATION_GRACE),
                ),
                StageWorkload::new(inputs, |item: StageWorkItem<String, u64>| async move {
                    let (_, _, payload) = item.into_parts();
                    tokio::time::sleep(Duration::from_millis(
                        ORDERED_STAGE_ITEMS.saturating_sub(payload),
                    ))
                    .await;
                    Ok::<_, StageItemFailure>(payload)
                }),
                StageFold::new(
                    Vec::new(),
                    |ordered: &mut Vec<u64>, output: StageOutput<String, u64>| {
                        let (_, payload) = output.into_parts();
                        ordered.push(payload);
                        Ok(())
                    },
                ),
            );
            let ordered = context
                .stages()
                .execute(execution)
                .await
                .map_err(|_| PipelineFailure::new(PipelineStage::Parse))?;
            assert_eq!(ordered, (0..ORDERED_STAGE_ITEMS).collect::<Vec<_>>());
            context
                .progress()
                .begin_stage(PipelineStage::Copy)
                .await
                .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
            context
                .prepare_generation(GenerationContents::new(
                    staged,
                    canonical(GenerationFacts::default()),
                ))
                .await
                .map_err(|_| PipelineFailure::new(PipelineStage::Copy))
        })
        .await;
    let current = match current {
        Ok(current) => current,
        Err(error) => panic!("bounded ordered stage failed before publication: {error}"),
    };
    assert_eq!(current.generation_id(), &generation_id);
    let status = supervisor.status().await;
    assert_eq!(status.state(), SupervisorState::Completed);
    assert_eq!(status.completed_items(), ORDERED_STAGE_ITEMS);
    assert_eq!(
        status.completed_bytes(),
        ORDERED_STAGE_ITEMS * ORDERED_STAGE_ITEM_BYTES
    );
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

fn assert_cache_drift_failure(
    result: &Result<CurrentGeneration, SupervisorError>,
    storage: CacheProbeStorage,
) {
    let Err(SupervisorError::PipelineWithFileFailure { stage, failure }) = result else {
        panic!("{storage:?} cache drift returned the wrong failure: {result:?}");
    };
    assert_eq!(*stage, PipelineStage::Parse);
    assert_eq!(failure.path().as_str(), "src/cache_probe.rs");
    assert_eq!(
        failure.reason(),
        PipelineFailureReason::SourceChangedDuringParse
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn native_source_pipeline_copies_publishes_and_is_bm25_searchable() {
    let directory = match tempfile::tempdir() {
        Ok(directory) => directory,
        Err(error) => panic!("could not create native pipeline fixture: {error}"),
    };
    write_native_live_project(directory.path());
    let source_root = match SourceRoot::open(directory.path()) {
        Ok(source_root) => source_root,
        Err(error) => panic!("could not open native pipeline fixture: {error}"),
    };
    let pipeline = native_pipeline_config();
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), boundary_config());
    let current = supervisor
        .run(
            request_with_duration(target.clone(), BOUNDARY_LEASE_DURATION),
            move |context| async move {
                let native = build_native_generation(&context.stages(), source_root, pipeline)
                    .await
                    .map_err(|_| PipelineFailure::new(PipelineStage::Reduce))?;
                assert_eq!(native.report().discovered_files(), NATIVE_EXPECTED_FILES);
                assert!(native.report().symbols() >= NATIVE_EXPECTED_MINIMUM_SYMBOLS);
                assert!(
                    native.report().resolved_references()
                        >= NATIVE_EXPECTED_MINIMUM_RESOLVED_REFERENCES
                );
                let (facts, _) = native.into_parts();
                context
                    .progress()
                    .begin_stage(PipelineStage::Copy)
                    .await
                    .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
                context
                    .prepare_generation(GenerationContents::new(staged, facts))
                    .await
                    .map_err(|_| PipelineFailure::new(PipelineStage::Copy))
            },
        )
        .await;
    let current = match current {
        Ok(current) => current,
        Err(error) => panic!("native pipeline failed before publication: {error}"),
    };
    assert_eq!(current.generation_id(), &generation_id);
    assert_native_edge_kind(&fixture, &generation_id, EdgeKind::Instantiates).await;
    assert_native_unresolved_reference(&fixture, &generation_id, "format").await;
    assert_native_retrieval_and_coverage(&fixture, &generation_id).await;
    assert_native_framework_findings(&fixture).await;
    assert_eq!(
        supervisor.status().await.state(),
        SupervisorState::Completed
    );
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn postgres_spill_matches_memory_digest_centrality_and_publication() {
    let directory = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("could not create spill parity fixture: {error}"));
    write_native_live_project(directory.path());
    let fixture = open_fixture().await;

    let memory_staged = begin_generation(&fixture).await;
    let memory_generation_id = memory_staged.generation_id().clone();
    let memory_target = target(&fixture.project, &memory_generation_id);
    let memory_source = open_parity_source(directory.path(), "memory");
    let memory_supervisor = IndexerSupervisor::new(fixture.database.clone(), boundary_config());
    let (memory_report_sender, memory_report_receiver) = oneshot::channel();
    let memory_current = memory_supervisor
        .run(
            request_with_duration(memory_target, BOUNDARY_LEASE_DURATION),
            move |context| async move {
                let native = build_native_generation(
                    &context.stages(),
                    memory_source,
                    native_pipeline_config(),
                )
                .await
                .map_err(|error| {
                    error.reason().map_or_else(
                        || PipelineFailure::new(error.stage()),
                        |reason| PipelineFailure::with_reason(error.stage(), reason),
                    )
                })?;
                let report = native.report();
                let (facts, _) = native.into_parts();
                memory_report_sender
                    .send(report)
                    .map_err(|_| PipelineFailure::new(PipelineStage::Reduce))?;
                context
                    .progress()
                    .begin_stage(PipelineStage::Copy)
                    .await
                    .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
                context
                    .prepare_generation(GenerationContents::new(memory_staged, facts))
                    .await
                    .map_err(|_| PipelineFailure::new(PipelineStage::Copy))
            },
        )
        .await
        .unwrap_or_else(|error| panic!("memory parity generation failed: {error}"));
    let memory_report = memory_report_receiver
        .await
        .unwrap_or_else(|error| panic!("memory parity report missing: {error}"));
    let spill_staged = begin_generation(&fixture).await;
    let spill_generation_id = spill_staged.generation_id().clone();
    let spill_target = target(&fixture.project, &spill_generation_id);
    let spill_source = open_parity_source(directory.path(), "spill");
    let spill_build = cold_spill_build(&fixture, spill_source);
    let spill_supervisor =
        IndexerSupervisor::new(fixture.database.clone(), spill_parity_supervisor_config());
    let (spill_report_sender, spill_report_receiver) = oneshot::channel();
    let spill_current = spill_supervisor
        .run(
            request_with_duration(spill_target, SPILL_PARITY_LEASE_DURATION),
            move |context| async move {
                let spill = context
                    .generation_spill(&spill_staged, NativeGenerationSpillPolicy::default())
                    .map_err(|_| PipelineFailure::new(PipelineStage::Parse))?;
                let native = build_native_generation_spilled(&context.stages(), spill_build, spill)
                    .await
                    .map_err(|_| PipelineFailure::new(PipelineStage::Reduce))?;
                let report = native.report();
                let (digest, _) = native.into_parts();
                spill_report_sender
                    .send(report)
                    .map_err(|_| PipelineFailure::new(PipelineStage::Reduce))?;
                context
                    .progress()
                    .begin_stage(PipelineStage::Copy)
                    .await
                    .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
                context
                    .prepare_spilled_generation(SpilledGenerationContents::new(
                        spill_staged,
                        digest,
                    ))
                    .await
                    .map_err(|_| PipelineFailure::new(PipelineStage::Copy))
            },
        )
        .await
        .unwrap_or_else(|error| panic!("spill parity generation failed: {error}"));
    let spill_report = spill_report_receiver
        .await
        .unwrap_or_else(|error| panic!("spill parity report missing: {error}"));
    assert_eq!(
        memory_current.content_digest(),
        spill_current.content_digest()
    );
    let parity_ids = (&memory_generation_id, &spill_generation_id);
    let parity_reports = (&memory_report, &spill_report);
    assert_spill_parity_evidence(&fixture, parity_ids, parity_reports).await;
    assert_spill_progress_observable(&memory_supervisor, &spill_supervisor).await;

    fixture.close().await;
}

#[derive(Clone, Copy, Debug)]
enum CacheProbeStorage {
    Memory,
    PostgreSql,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn cache_hit_revalidation_preserves_source_drift_in_memory_and_postgres_spill() {
    let directory = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("could not create cache revalidation fixture: {error}"));
    write_cache_probe_project(directory.path(), CACHE_PROBE_ORIGINAL);
    let fixture = open_fixture().await;
    publish_cache_probe(&fixture, directory.path()).await;
    assert_parse_cache_populated(&fixture).await;

    for storage in [CacheProbeStorage::Memory, CacheProbeStorage::PostgreSql] {
        write_cache_probe_project(directory.path(), CACHE_PROBE_ORIGINAL);
        let staged = begin_generation(&fixture).await;
        let generation_id = staged.generation_id().clone();
        let target = target(&fixture.project, &generation_id);
        let source = SourceRoot::open(directory.path())
            .unwrap_or_else(|error| panic!("could not reopen {storage:?} cache fixture: {error}"));
        let cache = NativeParseCache::new(fixture.database.clone(), fixture.project.clone());
        let build = NativeGenerationBuild::new(source, native_spill_pipeline_config())
            .with_parse_cache(cache);
        let lock_statement = format!(
            r#"LOCK TABLE "{}"."native_parse_cache" IN ACCESS EXCLUSIVE MODE"#,
            fixture.schema
        );
        let mut table_lock =
            fixture.pool.begin().await.unwrap_or_else(|error| {
                panic!("{storage:?} cache lock transaction failed: {error}")
            });
        query(AssertSqlSafe(lock_statement))
            .execute(&mut *table_lock)
            .await
            .unwrap_or_else(|error| panic!("{storage:?} cache table lock failed: {error}"));
        let supervisor =
            IndexerSupervisor::new(fixture.database.clone(), spill_parity_supervisor_config());
        let supervisor_request = request_with_duration(target.clone(), SPILL_PARITY_LEASE_DURATION);
        let handle = tokio::spawn(async move {
            supervisor
                .run(supervisor_request, move |context| async move {
                    match storage {
                        CacheProbeStorage::Memory => {
                            let native = build_native_generation_with_scip_and_cache(
                                &context.stages(),
                                build,
                            )
                            .await
                            .map_err(|error| native_pipeline_failure(&error))?;
                            let (facts, _) = native.into_parts();
                            context
                                .progress()
                                .begin_stage(PipelineStage::Copy)
                                .await
                                .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
                            context
                                .prepare_generation(GenerationContents::new(staged, facts))
                                .await
                                .map_err(|_| PipelineFailure::new(PipelineStage::Copy))
                        }
                        CacheProbeStorage::PostgreSql => {
                            let spill = context
                                .generation_spill(&staged, NativeGenerationSpillPolicy::default())
                                .map_err(|_| PipelineFailure::new(PipelineStage::Parse))?;
                            let native =
                                build_native_generation_spilled(&context.stages(), build, spill)
                                    .await
                                    .map_err(|error| native_pipeline_failure(&error))?;
                            let (digest, _) = native.into_parts();
                            context
                                .progress()
                                .begin_stage(PipelineStage::Copy)
                                .await
                                .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
                            context
                                .prepare_spilled_generation(SpilledGenerationContents::new(
                                    staged, digest,
                                ))
                                .await
                                .map_err(|_| PipelineFailure::new(PipelineStage::Copy))
                        }
                    }
                })
                .await
        });
        wait_for_schema_lock(&fixture.pool, &fixture.schema, "native_parse_cache").await;
        write_cache_probe_project(directory.path(), CACHE_PROBE_CHANGED);
        table_lock
            .rollback()
            .await
            .unwrap_or_else(|error| panic!("{storage:?} cache lock rollback failed: {error}"));
        let result = tokio::time::timeout(SPILL_PARITY_OPERATION_TIMEOUT, handle)
            .await
            .unwrap_or_else(|error| panic!("{storage:?} cache build timed out: {error}"))
            .unwrap_or_else(|error| panic!("{storage:?} cache build task failed: {error}"));
        assert_cache_drift_failure(&result, storage);
        assert_generation_state(&fixture, &generation_id, GenerationState::Failed).await;
        assert!(matches!(
            fixture.database.lease_status(&target).await,
            Ok(None)
        ));
    }

    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn postgres_spill_in_loop_storage_fault_is_not_attributed_to_a_source_file() {
    let directory = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("could not create spill fault fixture: {error}"));
    write_spill_batch_project(directory.path());
    let fixture = open_fixture().await;
    install_spill_batch_failure(&fixture).await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let source = open_parity_source(directory.path(), "spill fault");
    let build = NativeGenerationBuild::new(source, native_spill_pipeline_config());
    let supervisor =
        IndexerSupervisor::new(fixture.database.clone(), spill_parity_supervisor_config());
    let (error_sender, error_receiver) = oneshot::channel();
    let result = supervisor
        .run(
            request_with_duration(target.clone(), SPILL_PARITY_LEASE_DURATION),
            move |context| async move {
                let spill = context
                    .generation_spill(&staged, NativeGenerationSpillPolicy::default())
                    .map_err(|_| PipelineFailure::new(PipelineStage::Parse))?;
                match build_native_generation_spilled(&context.stages(), build, spill).await {
                    Ok(native) => {
                        let (digest, _) = native.into_parts();
                        context
                            .progress()
                            .begin_stage(PipelineStage::Copy)
                            .await
                            .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
                        context
                            .prepare_spilled_generation(SpilledGenerationContents::new(
                                staged, digest,
                            ))
                            .await
                            .map_err(|_| PipelineFailure::new(PipelineStage::Copy))
                    }
                    Err(error) => {
                        let failure = native_pipeline_failure(&error);
                        let _ = error_sender.send(error);
                        Err(failure)
                    }
                }
            },
        )
        .await;
    let native_error = error_receiver
        .await
        .unwrap_or_else(|error| panic!("spill fault omitted its native failure: {error}"));

    assert!(matches!(
        native_error,
        NativePipelineError::Spill {
            stage: PipelineStage::Parse
        }
    ));
    assert!(native_error.file_failure().is_none());
    assert!(matches!(
        result,
        Err(SupervisorError::Pipeline {
            stage: PipelineStage::Parse
        })
    ));
    assert_generation_state(&fixture, &generation_id, GenerationState::Failed).await;
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn postgres_spill_item_deadline_remains_deadline_without_file_attribution() {
    let directory = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("could not create spill deadline fixture: {error}"));
    write_spill_batch_project(directory.path());
    let fixture = open_fixture().await;
    install_spill_batch_delay(&fixture).await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let source = open_parity_source(directory.path(), "spill deadline");
    let config =
        native_pipeline_config_with_deadlines(SPILL_ITEM_DEADLINE, SPILL_PARITY_STAGE_TIMEOUT);
    let build = NativeGenerationBuild::new(source, config);
    let supervisor =
        IndexerSupervisor::new(fixture.database.clone(), spill_deadline_supervisor_config());
    let observer_pool = fixture.pool.clone();
    let observer_schema = fixture.schema.clone();
    let (error_sender, error_receiver) = oneshot::channel();
    let result = supervisor
        .run(
            request_with_duration(target.clone(), SPILL_PARITY_LEASE_DURATION),
            move |context| async move {
                let spill = context
                    .generation_spill(&staged, NativeGenerationSpillPolicy::default())
                    .map_err(|_| PipelineFailure::new(PipelineStage::Parse))?;
                match build_native_generation_spilled(&context.stages(), build, spill).await {
                    Ok(native) => {
                        let (digest, _) = native.into_parts();
                        context
                            .progress()
                            .begin_stage(PipelineStage::Copy)
                            .await
                            .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
                        context
                            .prepare_spilled_generation(SpilledGenerationContents::new(
                                staged, digest,
                            ))
                            .await
                            .map_err(|_| PipelineFailure::new(PipelineStage::Copy))
                    }
                    Err(error) => {
                        let failure = native_pipeline_failure(&error);
                        let _ = error_sender.send(error);
                        wait_for_query_absent(
                            &observer_pool,
                            &observer_schema,
                            "%native_generation_spill_batches%",
                        )
                        .await;
                        Err(failure)
                    }
                }
            },
        )
        .await;
    let native_error = error_receiver
        .await
        .unwrap_or_else(|error| panic!("spill deadline omitted its native failure: {error}"));

    assert!(matches!(
        native_error,
        NativePipelineError::Stage(StageRunError::Item {
            stage: PipelineStage::Parse,
            kind: StageFailureKind::Deadline,
            ..
        })
    ));
    assert_eq!(
        native_error.reason(),
        Some(PipelineFailureReason::DeadlineExceeded)
    );
    assert!(native_error.file_failure().is_none());
    assert!(
        matches!(
            result,
            Err(SupervisorError::PipelineWithReason {
                stage: PipelineStage::Parse,
                reason: PipelineFailureReason::DeadlineExceeded
            })
        ),
        "spill deadline returned the wrong supervised failure: {result:?}"
    );
    assert_generation_state(&fixture, &generation_id, GenerationState::Failed).await;
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

async fn assert_spill_progress_observable(
    memory_supervisor: &IndexerSupervisor,
    spill_supervisor: &IndexerSupervisor,
) {
    let memory_status = memory_supervisor.status().await;
    let spill_status = spill_supervisor.status().await;
    assert!(
        spill_status.completed_items() > memory_status.completed_items(),
        "streamed resolution must report exact page and committed-batch progress before its outer work item completes"
    );
}

fn open_parity_source(path: &std::path::Path, strategy: &str) -> SourceRoot {
    SourceRoot::open(path)
        .unwrap_or_else(|error| panic!("could not open {strategy} parity source: {error}"))
}

fn cold_spill_build(fixture: &DatabaseFixture, source: SourceRoot) -> NativeGenerationBuild {
    let cache =
        NativeParseCache::new(fixture.database.clone(), fixture.project.clone()).with_reads(false);
    NativeGenerationBuild::new(source, native_spill_pipeline_config()).with_parse_cache(cache)
}

fn write_cache_probe_project(root: &std::path::Path, source: &str) {
    std::fs::create_dir_all(root.join(".git"))
        .unwrap_or_else(|error| panic!("could not create cache fixture root: {error}"));
    std::fs::create_dir_all(root.join("src"))
        .unwrap_or_else(|error| panic!("could not create cache fixture source: {error}"));
    std::fs::write(root.join("src/cache_probe.rs"), source)
        .unwrap_or_else(|error| panic!("could not write cache fixture source: {error}"));
}

async fn publish_cache_probe(fixture: &DatabaseFixture, root: &std::path::Path) {
    let staged = begin_generation(fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let source = SourceRoot::open(root)
        .unwrap_or_else(|error| panic!("could not open cache warmup source: {error}"));
    let cache = NativeParseCache::new(fixture.database.clone(), fixture.project.clone());
    let build =
        NativeGenerationBuild::new(source, native_spill_pipeline_config()).with_parse_cache(cache);
    let supervisor =
        IndexerSupervisor::new(fixture.database.clone(), spill_parity_supervisor_config());
    let current = supervisor
        .run(
            request_with_duration(target, SPILL_PARITY_LEASE_DURATION),
            move |context| async move {
                let native = build_native_generation_with_scip_and_cache(&context.stages(), build)
                    .await
                    .map_err(|error| native_pipeline_failure(&error))?;
                let (facts, _) = native.into_parts();
                context
                    .progress()
                    .begin_stage(PipelineStage::Copy)
                    .await
                    .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
                context
                    .prepare_generation(GenerationContents::new(staged, facts))
                    .await
                    .map_err(|_| PipelineFailure::new(PipelineStage::Copy))
            },
        )
        .await
        .unwrap_or_else(|error| panic!("cache warmup generation failed: {error}"));
    assert_eq!(current.generation_id(), &generation_id);
}

async fn assert_parse_cache_populated(fixture: &DatabaseFixture) {
    let statement = format!(
        r#"SELECT COUNT(*) AS entries
              FROM "{}"."native_parse_cache"
             WHERE project_id = CAST($1 AS uuid)"#,
        fixture.schema
    );
    let row = query(AssertSqlSafe(statement))
        .bind(fixture.project.as_str())
        .fetch_one(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("could not inspect cache warmup: {error}"));
    let entries = row
        .try_get::<i64, _>("entries")
        .unwrap_or_else(|error| panic!("cache warmup count was invalid: {error}"));
    assert!(entries > 0, "cache warmup did not persist an exact entry");
}

fn native_pipeline_failure(error: &NativePipelineError) -> PipelineFailure {
    if let Some(failure) = error.file_failure() {
        return PipelineFailure::with_file_failure(error.stage(), failure.clone());
    }
    error.reason().map_or_else(
        || PipelineFailure::new(error.stage()),
        |reason| PipelineFailure::with_reason(error.stage(), reason),
    )
}

fn write_spill_batch_project(root: &std::path::Path) {
    std::fs::create_dir_all(root.join(".git"))
        .unwrap_or_else(|error| panic!("could not create spill fault root: {error}"));
    std::fs::create_dir_all(root.join("src"))
        .unwrap_or_else(|error| panic!("could not create spill fault source: {error}"));
    for index in 0..SPILL_PARSE_BATCH_FILES {
        let source = format!("pub fn spill_probe_{index}() -> usize {{ {index} }}\n");
        std::fs::write(root.join(format!("src/spill_probe_{index:02}.rs")), source)
            .unwrap_or_else(|error| panic!("could not write spill fault source: {error}"));
    }
}

async fn install_spill_batch_failure(fixture: &DatabaseFixture) {
    let function = format!(
        r#"CREATE FUNCTION "{}"."fail_extracted_spill_batch"()
            RETURNS trigger
            LANGUAGE plpgsql
            AS $cartograph$
            BEGIN
                IF NEW.relation = 'extracted_files' THEN
                    RAISE EXCEPTION 'forced isolated spill append failure';
                END IF;
                RETURN NEW;
            END
            $cartograph$"#,
        fixture.schema
    );
    query(AssertSqlSafe(function))
        .execute(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("could not create spill fault function: {error}"));
    let trigger = format!(
        r#"CREATE TRIGGER fail_extracted_spill_batch
            BEFORE INSERT ON "{}"."native_generation_spill_batches"
            FOR EACH ROW
            EXECUTE FUNCTION "{}"."fail_extracted_spill_batch"()"#,
        fixture.schema, fixture.schema
    );
    query(AssertSqlSafe(trigger))
        .execute(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("could not create spill fault trigger: {error}"));
}

async fn install_spill_batch_delay(fixture: &DatabaseFixture) {
    let function = format!(
        r#"CREATE FUNCTION "{}"."delay_extracted_spill_batch"()
            RETURNS trigger
            LANGUAGE plpgsql
            AS $cartograph$
            BEGIN
                IF NEW.relation = 'extracted_files' THEN
                    PERFORM pg_sleep({SPILL_DELAY_SECONDS});
                END IF;
                RETURN NEW;
            END
            $cartograph$"#,
        fixture.schema
    );
    query(AssertSqlSafe(function))
        .execute(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("could not create spill delay function: {error}"));
    let trigger = format!(
        r#"CREATE TRIGGER delay_extracted_spill_batch
            BEFORE INSERT ON "{}"."native_generation_spill_batches"
            FOR EACH ROW
            EXECUTE FUNCTION "{}"."delay_extracted_spill_batch"()"#,
        fixture.schema, fixture.schema
    );
    query(AssertSqlSafe(trigger))
        .execute(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("could not create spill delay trigger: {error}"));
}

async fn assert_spill_parity_evidence(
    fixture: &DatabaseFixture,
    generation_ids: (&GenerationId, &GenerationId),
    reports: (&NativePipelineReport, &NativePipelineReport),
) {
    let (memory_generation_id, spill_generation_id) = generation_ids;
    assert_native_report_parity(reports.0, reports.1);
    assert_spilled_centrality_matches_memory(fixture, memory_generation_id, spill_generation_id)
        .await;
    assert_native_retrieval_and_coverage(fixture, spill_generation_id).await;
    assert_spill_work_was_collected(fixture, spill_generation_id).await;
}

fn assert_native_report_parity(memory: &NativePipelineReport, spill: &NativePipelineReport) {
    assert_eq!(memory.storage(), NativeGenerationStorage::Memory);
    assert_eq!(spill.storage(), NativeGenerationStorage::PostgreSql);
    assert!(memory.spill().is_none());
    assert!(spill.spill().is_some());
    assert_eq!(memory.discovered_files(), spill.discovered_files());
    assert_eq!(memory.source_bytes(), spill.source_bytes());
    assert_eq!(memory.symbols(), spill.symbols());
    assert_eq!(memory.numerical_sites(), spill.numerical_sites());
    assert_eq!(memory.resolved_references(), spill.resolved_references());
    assert_eq!(
        memory.unresolved_references(),
        spill.unresolved_references()
    );
    assert_eq!(memory.diagnostics(), spill.diagnostics());
    let cache = spill.parse_cache();
    assert_eq!(cache.hits(), 0);
    assert_eq!(cache.misses(), 0);
    assert_eq!(cache.bypassed(), spill.discovered_files());
    assert_eq!(cache.parsed_files(), spill.discovered_files());
    assert_eq!(cache.writes(), spill.discovered_files());
    assert_eq!(cache.corruptions(), 0);
    assert_eq!(cache.read_errors(), 0);
    assert_eq!(cache.write_errors(), 0);
}

async fn assert_spilled_centrality_matches_memory(
    fixture: &DatabaseFixture,
    memory_generation_id: &GenerationId,
    spill_generation_id: &GenerationId,
) {
    let statement = format!(
        r#"SELECT NOT EXISTS (
                (SELECT symbol_id, betweenness, pagerank
                   FROM "{schema}"."symbols"
                  WHERE project_id = CAST($1 AS uuid)
                    AND generation_id = CAST($2 AS uuid)
                 EXCEPT
                 SELECT symbol_id, betweenness, pagerank
                   FROM "{schema}"."symbols"
                  WHERE project_id = CAST($1 AS uuid)
                    AND generation_id = CAST($3 AS uuid))
                UNION ALL
                (SELECT symbol_id, betweenness, pagerank
                   FROM "{schema}"."symbols"
                  WHERE project_id = CAST($1 AS uuid)
                    AND generation_id = CAST($3 AS uuid)
                 EXCEPT
                 SELECT symbol_id, betweenness, pagerank
                   FROM "{schema}"."symbols"
                  WHERE project_id = CAST($1 AS uuid)
                    AND generation_id = CAST($2 AS uuid))
            ) AS identical"#,
        schema = fixture.schema,
    );
    let row = query(AssertSqlSafe(statement))
        .bind(fixture.project.as_str())
        .bind(memory_generation_id.as_str())
        .bind(spill_generation_id.as_str())
        .fetch_one(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("could not compare centrality: {error}"));
    assert!(row.try_get::<bool, _>("identical").unwrap_or(false));
}

async fn assert_spill_work_was_collected(fixture: &DatabaseFixture, generation_id: &GenerationId) {
    let statement = format!(
        r#"SELECT NOT EXISTS (
                SELECT 1 FROM "{schema}"."native_generation_spills"
                 WHERE project_id = CAST($1 AS uuid)
                   AND generation_id = CAST($2 AS uuid)
            ) AS collected"#,
        schema = fixture.schema,
    );
    let row = query(AssertSqlSafe(statement))
        .bind(fixture.project.as_str())
        .bind(generation_id.as_str())
        .fetch_one(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("could not inspect spill cleanup: {error}"));
    assert!(row.try_get::<bool, _>("collected").unwrap_or(false));
}

async fn assert_native_retrieval_and_coverage(
    fixture: &DatabaseFixture,
    generation_id: &GenerationId,
) {
    let hits = match fixture
        .database
        .search_current_code(SearchQuery::new(
            CurrentGenerationLookup::new(&fixture.project, generation_id),
            "Service",
            NATIVE_SEARCH_LIMIT,
        ))
        .await
    {
        Ok(hits) => hits,
        Err(error) => panic!("native generation BM25 search failed: {error}"),
    };
    assert!(hits.iter().any(|hit| {
        hit.generation_id() == generation_id && hit.qualified_name().contains("Service")
    }));
    assert_parser_only_bm25_hits(fixture, generation_id).await;
    assert_admitted_family_bm25_hits(fixture, generation_id).await;
    assert_generic_family_bm25_hits(fixture, generation_id).await;
    assert_custom_family_bm25_hits(fixture, generation_id).await;
    let imports = fixture
        .database
        .current_imports(&fixture.project, 50)
        .await
        .unwrap_or_else(|error| panic!("current import insights failed: {error}"));
    assert!(
        !imports.is_empty(),
        "native fixture import evidence must survive publication"
    );
    let dependency_coverage = fixture
        .database
        .current_dependency_coverage(&fixture.project, 50)
        .await
        .unwrap_or_else(|error| panic!("current dependency coverage failed: {error}"));
    assert!(
        !dependency_coverage.is_empty(),
        "published references must contribute dependency coverage"
    );
    let hotspots = fixture
        .database
        .current_structural_hotspots(&fixture.project, 100)
        .await
        .unwrap_or_else(|error| panic!("current hotspot insights failed: {error}"));
    let expected_files = usize::try_from(NATIVE_EXPECTED_FILES)
        .unwrap_or_else(|error| panic!("native file count does not fit usize: {error}"));
    assert_eq!(hotspots.len(), expected_files);
    let coverage = fixture
        .database
        .current_structural_coverage(&fixture.project, 50)
        .await
        .unwrap_or_else(|error| panic!("current structural coverage failed: {error}"));
    assert!(
        !coverage.is_empty(),
        "published symbols must be visible to structural coverage"
    );
    let dead_code = fixture
        .database
        .current_dead_code(&fixture.project, 50, false)
        .await
        .unwrap_or_else(|error| panic!("current dead-code insights failed: {error}"));
    assert!(
        !dead_code.is_empty(),
        "the deliberately disconnected fixture must produce dead-code candidates"
    );
}

async fn assert_native_framework_findings(fixture: &DatabaseFixture) {
    fixture
        .database
        .refresh_current_structural_findings(&fixture.project, Duration::from_secs(30))
        .await
        .unwrap_or_else(|error| panic!("structural finding refresh failed: {error}"));
    let unused_exports = fixture
        .database
        .query_current_structural_findings(
            &fixture.project,
            &StructuralFindingQuery::new(100)
                .and_then(|query| query.with_finding(Some("unused_export")))
                .map_or_else(
                    |error| panic!("unused-export query was invalid: {error}"),
                    |query| query.with_minimum_severity(StructuralFindingSeverity::Info),
                ),
        )
        .await
        .unwrap_or_else(|error| panic!("current structural findings failed: {error}"));
    for (path, name) in [
        ("app/about/page.tsx", "metadata"),
        ("app/api/things/route.ts", "GET"),
        ("app/api/things/route.ts", "runtime"),
        ("app/routes/dashboard.tsx", "loader"),
        ("app/routes/dashboard.tsx", "Dashboard"),
        ("src/routes/about.ts", "Route"),
        ("src/model.ts", "PublicRecord"),
        ("src/service.ts", "SessionTable"),
        ("src/service.ts", "greet"),
        ("src/service.ts", "renderPanel"),
        ("src/service.ts", "scheduled"),
        ("build.config.ts", "buildEnd"),
    ] {
        assert!(
            !has_structural_finding(&unused_exports, path, name),
            "framework-owned export was incorrectly reported: {path}::{name}; findings={unused_exports:?}"
        );
    }
    assert_type_only_external_consumer(fixture).await;
    for (path, name) in [
        ("app/about/page.tsx", "someHelper"),
        ("app/api/things/route.ts", "metadata"),
        ("app/api/things/route.ts", "buildResponse"),
        ("app/routes/dashboard.tsx", "formatDate"),
        ("src/routes/about.ts", "helper"),
        ("lib/action.ts", "action"),
        ("build.config.ts", "unusedConfigHelper"),
    ] {
        assert!(
            has_structural_finding(&unused_exports, path, name),
            "ordinary unused export was hidden: {path}::{name}; findings={unused_exports:?}"
        );
    }
    for local in ["escaped", "sequence_id"] {
        assert!(
            !has_structural_finding(&unused_exports, "src/rows.py", local),
            "function-local Python binding was reported as unused export: {local}; findings={unused_exports:?}"
        );
    }
    assert!(
        !has_structural_finding(&unused_exports, "lib/action.ts", "claimDue"),
        "declaration-only type member was reported as unused export: claimDue; findings={unused_exports:?}"
    );
    assert!(
        !has_structural_finding(&unused_exports, "src/service.ts", "RuntimeSchema"),
        "runtime use through an imported schema member was not resolved: RuntimeSchema; findings={unused_exports:?}"
    );
    let long_parameters = fixture
        .database
        .query_current_structural_findings(
            &fixture.project,
            &StructuralFindingQuery::new(100)
                .and_then(|query| query.with_finding(Some("long_parameter_list")))
                .map_or_else(
                    |error| panic!("long-parameter query was invalid: {error}"),
                    |query| query.with_minimum_severity(StructuralFindingSeverity::Info),
                ),
        )
        .await
        .unwrap_or_else(|error| panic!("long-parameter findings failed: {error}"));
    assert!(
        !has_structural_finding(&long_parameters, "generated/platform.d.ts", "execute",),
        "declaration-only generated API created implementation debt: {long_parameters:?}"
    );
    fixture
        .database
        .current_structural_finding_stats(&fixture.project)
        .await
        .unwrap_or_else(|error| panic!("current structural finding stats failed: {error}"));
}

async fn assert_type_only_external_consumer(fixture: &DatabaseFixture) {
    let statement = format!(
        r#"SELECT COUNT(DISTINCT source.file_id)::bigint AS incoming
            FROM "{schema}"."edges" AS edges
            JOIN "{schema}"."symbols" AS target
              ON target.project_id = edges.project_id
             AND target.generation_id = edges.generation_id
             AND target.symbol_id = edges.target_symbol_id
            JOIN "{schema}"."symbols" AS source
              ON source.project_id = edges.project_id
             AND source.generation_id = edges.generation_id
             AND source.symbol_id = edges.source_symbol_id
            WHERE edges.project_id = CAST($1 AS uuid)
              AND target.qualified_name = 'PublicRecord'
              AND source.file_id <> target.file_id"#,
        schema = fixture.schema,
    );
    let row = query(AssertSqlSafe(statement))
        .bind(fixture.project.as_str())
        .fetch_one(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("type-only consumer query failed: {error}"));
    let incoming = row
        .try_get::<i64, _>("incoming")
        .unwrap_or_else(|error| panic!("type-only consumer count was invalid: {error}"));
    assert_eq!(
        incoming, 1,
        "type-only export regression must prove one distinct external consumer file"
    );
}

fn write_native_live_project(root: &std::path::Path) {
    std::fs::create_dir(root.join(".git"))
        .unwrap_or_else(|error| panic!("could not create native .git fixture: {error}"));
    std::fs::create_dir_all(root.join("src"))
        .unwrap_or_else(|error| panic!("could not create native source fixture: {error}"));
    std::fs::write(
        root.join("src/service.ts"),
        "export interface Greeter { greet(): string; }\nexport class Service implements Greeter {\n  greet(): string { return format(); }\n}\nexport const RuntimeSchema = z.object({ value: z.string() });\nexport type RuntimeSchema = z.infer<typeof RuntimeSchema>;\nexport const SessionTable = table('session');\nexport const AuthSchema = { sessions: SessionTable };\nexport function renderPanel(): string { return 'panel'; }\nexport default { scheduled(): void {} };\n",
    )
    .unwrap_or_else(|error| panic!("could not write native service fixture: {error}"));
    std::fs::write(
        root.join("src/build.ts"),
        "import { RuntimeSchema, Service } from './service';\nexport function build(): Service { return new Service(); }\nexport function parseRuntime(value: unknown) { return RuntimeSchema.safeParse(value); }\nexport function runInterface(iface: Service): string { return iface.greet(); }\nexport function loadRenderer(mod: Record<string, () => string>): string { return mod[\"renderPanel\"](); }\n",
    )
    .unwrap_or_else(|error| panic!("could not write native build fixture: {error}"));
    write_type_consumer_fixture(root);
    for (path, source) in [
        (
            "app/about/page.tsx",
            "export const metadata = { title: 'Safe' };\nexport function someHelper(): string { return 'safe'; }\n",
        ),
        (
            "app/api/things/route.ts",
            "export async function GET(): Promise<Response> { return new Response('safe'); }\nexport const runtime = 'nodejs';\nexport const metadata = { title: 'not-a-route-convention' };\nexport function buildResponse(): Response { return new Response('safe'); }\n",
        ),
        (
            "app/routes/dashboard.tsx",
            "export async function loader(): Promise<unknown> { return null; }\nexport default function Dashboard() { return <main />; }\nexport function formatDate(): string { return 'safe'; }\n",
        ),
        (
            "src/routes/about.ts",
            "export const Route = createFileRoute('/about')({ component: About });\nexport const helper = 1;\n",
        ),
        (
            "lib/action.ts",
            "export type DeliveryStore = Readonly<{ claimDue(input: string): Promise<void> }>;\nexport function action(): string { return 'ordinary-unused-action'; }\n",
        ),
        (
            "generated/platform.d.ts",
            "// Generated file. Do not edit.\ninterface SyntheticPlatformApi {\n  execute(first: string, second: ArrayBuffer, third: unknown, fourth: unknown, fifth: unknown, sixth: boolean, seventh: string[]): Promise<unknown>;\n}\n",
        ),
        (
            "src/rows.py",
            "def build_rows(values):\n    escaped = [str(value).replace('|', '\\\\|') for value in values]\n    sequence_id = len(escaped)\n    return sequence_id, escaped\n",
        ),
        (
            "build.config.ts",
            "export default { buildEnd(): void {} };\nexport function unusedConfigHelper(): void {}\n",
        ),
    ] {
        let target = root.join(path);
        std::fs::create_dir_all(
            target
                .parent()
                .unwrap_or_else(|| panic!("framework export fixture had no parent: {path}")),
        )
        .unwrap_or_else(|error| panic!("could not create {path} parent: {error}"));
        std::fs::write(target, source)
            .unwrap_or_else(|error| panic!("could not write {path}: {error}"));
    }
    for fixtures in [
        &NATIVE_PARSER_ONLY_FIXTURES[..],
        &NATIVE_ADMITTED_FAMILY_FIXTURES[..],
        &NATIVE_GENERIC_FAMILY_FIXTURES[..],
        &NATIVE_CUSTOM_FAMILY_FIXTURES[..],
    ] {
        write_native_fixture_group(root, fixtures);
    }
}

fn write_native_fixture_group(root: &std::path::Path, fixtures: &[(&str, &str, &str, &str)]) {
    for &(path, source, _, _) in fixtures {
        let target = root.join(path);
        std::fs::create_dir_all(
            target
                .parent()
                .unwrap_or_else(|| panic!("native live fixture had no parent: {path}")),
        )
        .unwrap_or_else(|error| panic!("could not create {path} parent: {error}"));
        std::fs::write(target, source)
            .unwrap_or_else(|error| panic!("could not write {path}: {error}"));
    }
}

fn write_type_consumer_fixture(root: &std::path::Path) {
    std::fs::write(
        root.join("src/model.ts"),
        "export type PublicRecord = Readonly<{ id: string }>;\n",
    )
    .unwrap_or_else(|error| panic!("could not write type export fixture: {error}"));
    std::fs::write(
        root.join("src/consumer.ts"),
        "import type { PublicRecord } from './model';\nexport function readId(value: PublicRecord): string { return value.id; }\n",
    )
    .unwrap_or_else(|error| panic!("could not write type consumer fixture: {error}"));
}

fn has_structural_finding(
    findings: &[cartograph_db::StructuralFinding],
    path: &str,
    name: &str,
) -> bool {
    findings.iter().any(|finding| {
        finding.finding() == "unused_export"
            && finding.path() == path
            && finding
                .qualified_name()
                .rsplit([':', '.', '#', '/', '$'])
                .find(|component| !component.is_empty())
                == Some(name)
    })
}

async fn assert_parser_only_bm25_hits(fixture: &DatabaseFixture, generation_id: &GenerationId) {
    for (path, _, language, query) in NATIVE_PARSER_ONLY_FIXTURES {
        let hits = fixture
            .database
            .search_current_code(SearchQuery::new(
                CurrentGenerationLookup::new(&fixture.project, generation_id),
                query,
                NATIVE_SEARCH_LIMIT,
            ))
            .await
            .unwrap_or_else(|error| panic!("parser-only BM25 query failed for {path}: {error}"));
        assert!(
            hits.iter().any(|hit| {
                hit.generation_id() == generation_id
                    && hit.path() == path
                    && hit.language() == language
                    && hit.document_kind() == "file"
                    && hit.symbol_id().is_some()
                    && hit.qualified_name().is_empty()
                    && hit
                        .components()
                        .contains(&cartograph_db::SearchComponent::Code)
            }),
            "parser-only file document was not BM25 searchable: {path}"
        );
    }
}

async fn assert_admitted_family_bm25_hits(fixture: &DatabaseFixture, generation_id: &GenerationId) {
    for (path, _, language, query) in NATIVE_ADMITTED_FAMILY_FIXTURES {
        let hits = fixture
            .database
            .search_current_code(SearchQuery::new(
                CurrentGenerationLookup::new(&fixture.project, generation_id),
                query,
                NATIVE_SEARCH_LIMIT,
            ))
            .await
            .unwrap_or_else(|error| {
                panic!("admitted-family BM25 query failed for {path}: {error}")
            });
        assert!(
            hits.iter().any(|hit| {
                hit.generation_id() == generation_id
                    && hit.path() == path
                    && hit.language() == language
                    && hit.symbol_id().is_some()
                    && hit.document_kind() == "symbol"
            }),
            "admitted-family symbol document was not BM25 searchable: {path}"
        );
    }
}

async fn assert_generic_family_bm25_hits(fixture: &DatabaseFixture, generation_id: &GenerationId) {
    for (path, _, language, query) in NATIVE_GENERIC_FAMILY_FIXTURES {
        let hits = fixture
            .database
            .search_current_code(SearchQuery::new(
                CurrentGenerationLookup::new(&fixture.project, generation_id),
                query,
                NATIVE_SEARCH_LIMIT,
            ))
            .await
            .unwrap_or_else(|error| panic!("generic-family BM25 query failed for {path}: {error}"));
        assert!(
            hits.iter().any(|hit| {
                hit.generation_id() == generation_id
                    && hit.path() == path
                    && hit.language() == language
                    && hit.symbol_id().is_some()
                    && hit.document_kind() == "symbol"
            }),
            "generic-family symbol document was not BM25 searchable: {path}; hits={:?}",
            hits.iter()
                .map(|hit| {
                    (
                        hit.path(),
                        hit.language(),
                        hit.document_kind(),
                        hit.qualified_name(),
                    )
                })
                .collect::<Vec<_>>()
        );
    }
}

async fn assert_custom_family_bm25_hits(fixture: &DatabaseFixture, generation_id: &GenerationId) {
    for (path, _, language, query) in NATIVE_CUSTOM_FAMILY_FIXTURES {
        let hits = fixture
            .database
            .search_current_code(SearchQuery::new(
                CurrentGenerationLookup::new(&fixture.project, generation_id),
                query,
                NATIVE_SEARCH_LIMIT,
            ))
            .await
            .unwrap_or_else(|error| panic!("custom-family BM25 query failed for {path}: {error}"));
        assert!(
            hits.iter().any(|hit| {
                hit.generation_id() == generation_id
                    && hit.path() == path
                    && hit.language() == language
                    && hit.symbol_id().is_some()
                    && hit.document_kind() == "symbol"
            }),
            "custom-family symbol document was not BM25 searchable: {path}; hits={:?}",
            hits.iter()
                .map(|hit| {
                    (
                        hit.path(),
                        hit.language(),
                        hit.document_kind(),
                        hit.qualified_name(),
                    )
                })
                .collect::<Vec<_>>()
        );
    }
}

async fn assert_native_unresolved_reference(
    fixture: &DatabaseFixture,
    generation_id: &GenerationId,
    reference_name: &str,
) {
    let statement = format!(
        r#"SELECT owner_symbol_id IS NOT NULL AS has_owner,
                  target_symbol_id IS NULL AS unresolved,
                  resolution_provenance
            FROM "{schema}"."references"
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)
              AND reference_name = $3"#,
        schema = fixture.schema,
    );
    let row = query(AssertSqlSafe(statement))
        .bind(fixture.project.as_str())
        .bind(generation_id.as_str())
        .bind(reference_name)
        .fetch_one(&fixture.pool)
        .await
        .unwrap_or_else(|error| panic!("could not inspect unresolved reference: {error}"));
    assert!(row.try_get::<bool, _>("has_owner").unwrap_or(false));
    assert!(row.try_get::<bool, _>("unresolved").unwrap_or(false));
    assert!(matches!(
        row.try_get::<String, _>("resolution_provenance"),
        Ok(value) if value == "native-unresolved"
    ));
}

async fn assert_native_edge_kind(
    fixture: &DatabaseFixture,
    generation_id: &GenerationId,
    kind: EdgeKind,
) {
    let statement = format!(
        r#"SELECT EXISTS (
                SELECT 1 FROM "{schema}"."edges"
                WHERE project_id = CAST($1 AS uuid)
                  AND generation_id = CAST($2 AS uuid)
                  AND edge_kind = $3
            ) AS present"#,
        schema = fixture.schema,
    );
    let row = query(AssertSqlSafe(statement))
        .bind(fixture.project.as_str())
        .bind(generation_id.as_str())
        .bind(kind.as_str())
        .fetch_one(&fixture.pool)
        .await;
    assert!(matches!(row, Ok(row) if row.try_get::<bool, _>("present").unwrap_or(false)));
}

fn native_pipeline_config() -> NativePipelineConfig {
    native_pipeline_config_with_timeout(NATIVE_STAGE_TIMEOUT)
}

fn native_spill_pipeline_config() -> NativePipelineConfig {
    native_pipeline_config_with_timeout(SPILL_PARITY_STAGE_TIMEOUT)
}

fn native_pipeline_config_with_timeout(stage_timeout: Duration) -> NativePipelineConfig {
    native_pipeline_config_with_deadlines(stage_timeout, stage_timeout)
}

fn native_pipeline_config_with_deadlines(
    item_timeout: Duration,
    stage_timeout: Duration,
) -> NativePipelineConfig {
    let discovery = match DiscoveryLimits::new(NATIVE_MAX_FILES, NATIVE_MAX_PATH_BYTES) {
        Ok(discovery) => discovery,
        Err(error) => panic!("native test discovery limits were invalid: {error}"),
    };
    let source = match SourceLimits::new(NATIVE_MAX_SOURCE_BYTES) {
        Ok(source) => source,
        Err(error) => panic!("native test source limits were invalid: {error}"),
    };
    let retained =
        match NativeRetainedLimits::new(NATIVE_MAX_MANIFEST_BYTES, NATIVE_MAX_GENERATION_BYTES) {
            Ok(retained) => retained,
            Err(error) => panic!("native test retained limits were invalid: {error}"),
        };
    let limits = NativePipelineLimits::new(discovery, source, retained);
    let capacity = StageCapacity::new(WORKER_COUNT.into(), WORKER_COUNT.into());
    let parallelism = match NativePipelineParallelism::new(capacity, capacity) {
        Ok(parallelism) => parallelism,
        Err(error) => panic!("native test pipeline parallelism was invalid: {error}"),
    };
    let deadlines = match NativePipelineDeadlines::new(
        item_timeout,
        stage_timeout,
        STANDARD_CANCELLATION_GRACE,
    ) {
        Ok(deadlines) => deadlines,
        Err(error) => panic!("native test pipeline deadlines were invalid: {error}"),
    };
    NativePipelineConfig::new(limits, parallelism, deadlines)
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn large_payload_copy_uses_its_own_stage_deadline() {
    let fixture = open_fixture().await;
    install_copy_delay(&fixture).await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), large_payload_copy_config());
    let metrics = PrepareGenerationMetrics::new();
    let observed_metrics = metrics.clone();
    let current = supervisor
        .run(request(target.clone()), move |context| async move {
            context
                .progress()
                .begin_stage(PipelineStage::Copy)
                .await
                .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
            context
                .prepare_generation(
                    GenerationContents::new(
                        staged,
                        canonical(GenerationFacts {
                            documents: vec![large_copy_probe_document()],
                            ..GenerationFacts::default()
                        }),
                    )
                    .with_metrics(metrics),
                )
                .await
                .map_err(|_| PipelineFailure::new(PipelineStage::Copy))
        })
        .await;
    let current = match current {
        Ok(current) => current,
        Err(error) => panic!("large payload COPY used the heartbeat deadline: {error}"),
    };
    assert_eq!(current.generation_id(), &generation_id);
    assert_eq!(
        supervisor.status().await.state(),
        SupervisorState::Completed
    );
    assert!(
        observed_metrics.snapshot().copy_duration() > LARGE_COPY_HEARTBEAT_TIMEOUT,
        "COPY fixture did not exceed the heartbeat request deadline"
    );
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn recovered_ready_generation_still_publishes_through_supervisor_gate() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let staging_lease = match fixture
        .database
        .acquire_lease(LeaseRequest::new(
            target.clone(),
            LeaseOwner::new(process::id(), "recovered-ready-staging"),
            TEST_LEASE_DURATION,
        ))
        .await
    {
        Ok(lease) => lease,
        Err(error) => panic!("recovered-ready staging lease failed: {error}"),
    };
    let ready = match fixture
        .database
        .prepare_generation(
            GenerationContents::new(staged, canonical(GenerationFacts::default())),
            &staging_lease.fence(),
        )
        .await
    {
        Ok(ready) => ready,
        Err(error) => panic!("recovered-ready staging failed: {error}"),
    };
    assert!(fixture.database.release_lease(&staging_lease).await.is_ok());
    drop(ready);
    let ready = match fixture
        .database
        .recover_generation(GenerationRecoveryRequest::new(
            &fixture.project,
            &generation_id,
        ))
        .await
    {
        Ok(Some(cartograph_db::RecoverableGeneration::Ready(ready))) => ready,
        Ok(_) => panic!("ready generation was not recoverable for supervised publication"),
        Err(error) => panic!("ready generation recovery failed: {error}"),
    };
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), standard_config());
    let current = supervisor
        .run(request(target.clone()), move |_| async move { Ok(ready) })
        .await;
    let current = match current {
        Ok(current) => current,
        Err(error) => panic!("recovered ready generation did not publish: {error}"),
    };
    assert_eq!(current.generation_id(), &generation_id);
    assert_eq!(
        supervisor.status().await.state(),
        SupervisorState::Completed
    );
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn dropped_failed_child_blocks_publication_and_cleans_owned_generation() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), standard_config());
    let result = supervisor
        .run(request(target.clone()), move |context| async move {
            let failed_child = context
                .spawn(1, async {
                    Err::<(), PipelineFailure>(PipelineFailure::new(PipelineStage::Parse))
                })
                .map_err(|_| PipelineFailure::new(PipelineStage::Parse))?;
            drop(failed_child);
            tokio::task::yield_now().await;
            context
                .progress()
                .begin_stage(PipelineStage::Copy)
                .await
                .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
            context
                .prepare_generation(GenerationContents::new(
                    staged,
                    canonical(GenerationFacts::default()),
                ))
                .await
                .map_err(|_| PipelineFailure::new(PipelineStage::Copy))
        })
        .await;
    assert!(matches!(result, Err(SupervisorError::WorkerFailed)));
    assert_eq!(supervisor.status().await.state(), SupervisorState::Failed);
    assert_generation_state(&fixture, &generation_id, GenerationState::Failed).await;
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn propagated_pipeline_failure_precedes_the_observed_worker_poison_bit() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), standard_config());
    let result = supervisor
        .run(request(target.clone()), move |context| async move {
            drop(staged);
            let failed_child = context
                .spawn(1, async {
                    Err::<(), PipelineFailure>(PipelineFailure::new(PipelineStage::Parse))
                })
                .map_err(|_| PipelineFailure::new(PipelineStage::Parse))?;
            let _observed_failure = failed_child.join().await;
            Err::<ReadyGeneration, _>(PipelineFailure::new(PipelineStage::Parse))
        })
        .await;
    assert!(matches!(
        result,
        Err(SupervisorError::Pipeline {
            stage: PipelineStage::Parse
        })
    ));
    assert_eq!(supervisor.status().await.state(), SupervisorState::Failed);
    assert_generation_state(&fixture, &generation_id, GenerationState::Failed).await;
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn blocked_supervised_copy_rolls_back_backend_query_and_advisory_locks() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let lock_statement = format!(
        r#"LOCK TABLE "{}"."search_documents" IN ACCESS EXCLUSIVE MODE"#,
        fixture.schema
    );
    let mut table_lock = match fixture.pool.begin().await {
        Ok(transaction) => transaction,
        Err(error) => panic!("COPY lock transaction failed: {error}"),
    };
    if let Err(error) = query(AssertSqlSafe(lock_statement))
        .execute(&mut *table_lock)
        .await
    {
        panic!("COPY table lock failed: {error}");
    }
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), blocked_copy_config());
    let result = tokio::time::timeout(
        ABORT_RESULT_BOUND,
        supervisor.run(request(target.clone()), move |context| async move {
            context
                .prepare_generation(GenerationContents::new(
                    staged,
                    canonical(GenerationFacts {
                        documents: vec![copy_probe_document()],
                        ..GenerationFacts::default()
                    }),
                ))
                .await
                .map_err(|_| PipelineFailure::new(PipelineStage::Copy))
        }),
    )
    .await;
    let result = result.unwrap_or_else(|error| {
        panic!("blocked supervised COPY exceeded its absolute deadline: {error}")
    });
    let expected_copy_failure = matches!(
        &result,
        Err(SupervisorError::Pipeline {
            stage: PipelineStage::Copy
        })
    );
    assert_no_active_schema_work(&fixture).await;
    assert_generation_advisories_available(&fixture, &target).await;
    assert!(table_lock.rollback().await.is_ok());
    assert_generation_state(&fixture, &generation_id, GenerationState::Failed).await;
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
    assert!(
        expected_copy_failure,
        "unexpected blocked COPY result: {result:?}"
    );
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn requested_cancellation_reaps_inflight_copy_before_external_unlock() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let lock_statement = format!(
        r#"LOCK TABLE "{}"."search_documents" IN ACCESS EXCLUSIVE MODE"#,
        fixture.schema
    );
    let mut table_lock = match fixture.pool.begin().await {
        Ok(transaction) => transaction,
        Err(error) => panic!("cancelled COPY lock transaction failed: {error}"),
    };
    if let Err(error) = query(AssertSqlSafe(lock_statement))
        .execute(&mut *table_lock)
        .await
    {
        panic!("cancelled COPY table lock failed: {error}");
    }
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), copy_cancel_config());
    let runner = supervisor.clone();
    let request_target = target.clone();
    let (copy_gate, copy_control) = copy_start_barrier();
    let handle = tokio::spawn(async move {
        runner
            .run(request(request_target), move |context| async move {
                context
                    .progress()
                    .begin_stage(PipelineStage::Copy)
                    .await
                    .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
                copy_gate.wait().await?;
                let prepared = context
                    .prepare_generation(GenerationContents::new(
                        staged,
                        canonical(GenerationFacts {
                            documents: vec![copy_probe_document()],
                            ..GenerationFacts::default()
                        }),
                    ))
                    .await;
                tokio::time::sleep(COPY_CANCEL_NONCOOPERATIVE_TAIL).await;
                prepared.map_err(|_| PipelineFailure::new(PipelineStage::Copy))
            })
            .await
    });
    wait_for_lease(&fixture.database, &target).await;
    if let Err(error) = release_copy_and_wait_for_schema_lock(
        copy_control,
        &supervisor,
        &fixture.pool,
        &fixture.schema,
    )
    .await
    {
        let outcome = handle
            .await
            .unwrap_or_else(|join_error| panic!("COPY supervisor task failed: {join_error}"));
        panic!("{error}; supervisor outcome: {outcome:?}");
    }
    assert!(supervisor.cancel());
    let joined = tokio::time::timeout(ABORT_RESULT_BOUND, handle)
        .await
        .unwrap_or_else(|error| {
            panic!("cancelled COPY waited for the external table lock: {error}")
        });
    let result =
        joined.unwrap_or_else(|error| panic!("cancelled COPY supervisor task failed: {error}"));
    assert!(matches!(
        result,
        Err(SupervisorError::Cancelled {
            reason: CancellationReason::Requested,
            grace_exceeded: true
        })
    ));
    assert_no_active_schema_work(&fixture).await;
    assert_generation_advisories_available(&fixture, &target).await;
    assert!(table_lock.rollback().await.is_ok());
    assert_generation_state(&fixture, &generation_id, GenerationState::Failed).await;
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn aborting_public_run_reaps_inflight_copy_before_external_unlock() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let lock_statement = format!(
        r#"LOCK TABLE "{}"."search_documents" IN ACCESS EXCLUSIVE MODE"#,
        fixture.schema
    );
    let mut table_lock = match fixture.pool.begin().await {
        Ok(transaction) => transaction,
        Err(error) => panic!("aborted caller COPY lock transaction failed: {error}"),
    };
    if let Err(error) = query(AssertSqlSafe(lock_statement))
        .execute(&mut *table_lock)
        .await
    {
        panic!("aborted caller COPY table lock failed: {error}");
    }
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), copy_cancel_config());
    let runner = supervisor.clone();
    let request_target = target.clone();
    let (copy_gate, copy_control) = copy_start_barrier();
    let outer = tokio::spawn(async move {
        runner
            .run(request(request_target), move |context| async move {
                context
                    .progress()
                    .begin_stage(PipelineStage::Copy)
                    .await
                    .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
                copy_gate.wait().await?;
                let prepared = context
                    .prepare_generation(GenerationContents::new(
                        staged,
                        canonical(GenerationFacts {
                            documents: vec![copy_probe_document()],
                            ..GenerationFacts::default()
                        }),
                    ))
                    .await;
                tokio::time::sleep(COPY_CANCEL_NONCOOPERATIVE_TAIL).await;
                prepared.map_err(|_| PipelineFailure::new(PipelineStage::Copy))
            })
            .await
    });
    wait_for_lease(&fixture.database, &target).await;
    if let Err(error) = release_copy_and_wait_for_schema_lock(
        copy_control,
        &supervisor,
        &fixture.pool,
        &fixture.schema,
    )
    .await
    {
        let outcome = outer
            .await
            .unwrap_or_else(|join_error| panic!("COPY supervisor task failed: {join_error}"));
        panic!("{error}; supervisor outcome: {outcome:?}");
    }
    outer.abort();
    assert!(matches!(outer.await, Err(error) if error.is_cancelled()));
    wait_for_supervisor_state(&supervisor, SupervisorState::Wedged).await;
    assert_no_active_schema_work(&fixture).await;
    assert_generation_advisories_available(&fixture, &target).await;
    assert_generation_state(&fixture, &generation_id, GenerationState::Failed).await;
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));
    assert!(table_lock.rollback().await.is_ok());

    fixture.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn dropping_polled_run_outside_runtime_reaps_inflight_copy() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let lock_statement = format!(
        r#"LOCK TABLE "{}"."search_documents" IN ACCESS EXCLUSIVE MODE"#,
        fixture.schema
    );
    let mut table_lock = match fixture.pool.begin().await {
        Ok(transaction) => transaction,
        Err(error) => panic!("cross-thread drop lock transaction failed: {error}"),
    };
    if let Err(error) = query(AssertSqlSafe(lock_statement))
        .execute(&mut *table_lock)
        .await
    {
        panic!("cross-thread drop table lock failed: {error}");
    }
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), copy_cancel_config());
    let runner = supervisor.clone();
    let request_target = target.clone();
    let (copy_gate, copy_control) = copy_start_barrier();
    let mut run = Box::pin(async move {
        runner
            .run(request(request_target), move |context| async move {
                context
                    .progress()
                    .begin_stage(PipelineStage::Copy)
                    .await
                    .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
                copy_gate.wait().await?;
                let prepared = context
                    .prepare_generation(GenerationContents::new(
                        staged,
                        canonical(GenerationFacts {
                            documents: vec![copy_probe_document()],
                            ..GenerationFacts::default()
                        }),
                    ))
                    .await;
                tokio::time::sleep(COPY_CANCEL_NONCOOPERATIVE_TAIL).await;
                prepared.map_err(|_| PipelineFailure::new(PipelineStage::Copy))
            })
            .await
    });
    poll_fn(|context| {
        assert!(matches!(run.as_mut().poll(context), Poll::Pending));
        Poll::Ready(())
    })
    .await;
    wait_for_lease(&fixture.database, &target).await;
    if let Err(error) = release_copy_and_wait_for_schema_lock(
        copy_control,
        &supervisor,
        &fixture.pool,
        &fixture.schema,
    )
    .await
    {
        let outcome = run.await;
        panic!("{error}; supervisor outcome: {outcome:?}");
    }
    let dropper = std::thread::spawn(move || drop(run));
    assert!(dropper.join().is_ok());
    wait_for_supervisor_state(&supervisor, SupervisorState::Wedged).await;
    assert_no_active_schema_work(&fixture).await;
    assert_generation_advisories_available(&fixture, &target).await;
    assert_generation_state(&fixture, &generation_id, GenerationState::Failed).await;
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));
    assert!(table_lock.rollback().await.is_ok());

    fixture.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn requested_cancellation_fails_owned_generation_and_releases_lease() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), standard_config());
    let runner = supervisor.clone();
    let request_target = target.clone();
    let handle = tokio::spawn(async move {
        runner
            .run(request(request_target), move |context| async move {
                drop(staged);
                assert!(
                    context
                        .progress()
                        .begin_stage(PipelineStage::Read)
                        .await
                        .is_ok()
                );
                let mut cancellation = context.cancellation();
                cancellation.cancelled().await;
                Err::<ReadyGeneration, _>(PipelineFailure::new(PipelineStage::Read))
            })
            .await
    });
    wait_for_lease(&fixture.database, &target).await;
    assert!(supervisor.cancel());
    let result = join(handle).await;
    assert!(matches!(
        result,
        Err(SupervisorError::Cancelled {
            reason: CancellationReason::Requested,
            grace_exceeded: false
        })
    ));
    assert_eq!(
        supervisor.status().await.state(),
        SupervisorState::Cancelled
    );
    assert!(supervisor.status().await.heartbeat_count() > 0);
    assert_generation_state(&fixture, &generation_id, GenerationState::Failed).await;
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn progress_stall_cancels_work_and_marks_generation_failed() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), stalled_config());
    let result = supervisor
        .run(request(target.clone()), move |context| async move {
            drop(staged);
            let mut cancellation = context.cancellation();
            cancellation.cancelled().await;
            Err::<ReadyGeneration, _>(PipelineFailure::new(PipelineStage::Discover))
        })
        .await;
    assert!(matches!(
        result,
        Err(SupervisorError::Cancelled {
            reason: CancellationReason::ProgressStalled,
            grace_exceeded: false
        })
    ));
    let status = supervisor.status().await;
    assert_eq!(status.state(), SupervisorState::Wedged);
    assert_eq!(
        status.cancellation_reason(),
        Some(CancellationReason::ProgressStalled)
    );
    assert_generation_state(&fixture, &generation_id, GenerationState::Failed).await;
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn lost_lease_cancels_without_mutating_new_owners_generation() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), standard_config());
    let runner = supervisor.clone();
    let request_target = target.clone();
    let handle = tokio::spawn(async move {
        runner
            .run(request(request_target), move |context| async move {
                drop(staged);
                assert!(
                    context
                        .progress()
                        .begin_stage(PipelineStage::Parse)
                        .await
                        .is_ok()
                );
                let mut cancellation = context.cancellation();
                cancellation.cancelled().await;
                Err::<ReadyGeneration, _>(PipelineFailure::new(PipelineStage::Parse))
            })
            .await
    });
    wait_for_lease(&fixture.database, &target).await;
    expire_lease(&fixture, &target).await;
    let takeover = match fixture
        .database
        .acquire_lease(LeaseRequest::new(
            target.clone(),
            LeaseOwner::new(process::id(), "takeover-owner"),
            TEST_LEASE_DURATION,
        ))
        .await
    {
        Ok(lease) => lease,
        Err(error) => panic!("takeover lease acquisition failed: {error}"),
    };
    let result = join(handle).await;
    assert!(matches!(
        result,
        Err(SupervisorError::Cancelled {
            reason: CancellationReason::LeaseLost,
            grace_exceeded: false
        })
    ));
    assert_generation_state(&fixture, &generation_id, GenerationState::Staging).await;
    let status = match fixture.database.lease_status(&target).await {
        Ok(Some(status)) => status,
        Ok(None) => panic!("takeover lease disappeared"),
        Err(error) => panic!("takeover lease status failed: {error}"),
    };
    assert_eq!(status.owner_process_start(), "takeover-owner");
    assert!(fixture.database.release_lease(&takeover).await.is_ok());
    fail_recoverable_generation(&fixture, &generation_id).await;

    fixture.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn operation_deadline_cancels_despite_continuous_progress() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), deadline_config());
    let result = supervisor
        .run(request(target.clone()), move |context| async move {
            drop(staged);
            assert!(
                context
                    .progress()
                    .begin_stage(PipelineStage::Resolve)
                    .await
                    .is_ok()
            );
            let mut cancellation = context.cancellation();
            loop {
                if cancellation.is_cancelled() {
                    cancellation.cancelled().await;
                    return Err(PipelineFailure::new(PipelineStage::Resolve));
                }
                assert!(context.progress().advance(1, 1).await.is_ok());
                tokio::task::yield_now().await;
            }
        })
        .await;
    assert!(
        matches!(
            result,
            Err(SupervisorError::Cancelled {
                reason: CancellationReason::OperationDeadline,
                grace_exceeded: false
            })
        ),
        "unexpected operation-deadline result: {result:?}"
    );
    assert_eq!(
        supervisor.status().await.state(),
        SupervisorState::Cancelled
    );
    assert!(supervisor.status().await.heartbeat_count() > 0);
    assert_generation_state(&fixture, &generation_id, GenerationState::Failed).await;
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn noncooperative_work_is_dropped_after_visible_cancellation_grace() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let dropped = Arc::new(AtomicBool::new(false));
    let drop_observer = dropped.clone();
    let supervisor = IndexerSupervisor::new(
        fixture.database.clone(),
        standard_config().with_cancellation_grace(SHORT_CANCELLATION_GRACE),
    );
    let runner = supervisor.clone();
    let request_target = target.clone();
    let handle = tokio::spawn(async move {
        runner
            .run(request(request_target), move |context| async move {
                drop(staged);
                let _drop_flag = DropFlag(drop_observer);
                assert!(
                    context
                        .progress()
                        .begin_stage(PipelineStage::Read)
                        .await
                        .is_ok()
                );
                tokio::time::sleep(NONCOOPERATIVE_WORK_DURATION).await;
                Err(PipelineFailure::new(PipelineStage::Read))
            })
            .await
    });
    wait_for_lease(&fixture.database, &target).await;
    assert!(supervisor.cancel());
    tokio::time::sleep(CANCELLING_OBSERVATION_DELAY).await;
    assert_eq!(
        supervisor.status().await.state(),
        SupervisorState::Cancelling
    );
    let result = join(handle).await;
    assert!(matches!(
        result,
        Err(SupervisorError::Cancelled {
            reason: CancellationReason::Requested,
            grace_exceeded: true
        })
    ));
    let status = supervisor.status().await;
    assert_eq!(status.state(), SupervisorState::Wedged);
    assert!(status.grace_exceeded());
    assert!(dropped.load(Ordering::Acquire));
    assert_generation_state(&fixture, &generation_id, GenerationState::Failed).await;
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn lease_heartbeats_continue_while_root_work_blocks_its_thread() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), blocking_work_config());
    let runner = supervisor.clone();
    let request_target = target.clone();
    let (blocking, blocking_started) = oneshot::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let handle = tokio::spawn(async move {
        runner
            .run(request(request_target), move |context| async move {
                drop(staged);
                assert!(
                    context
                        .progress()
                        .begin_stage(PipelineStage::Resolve)
                        .await
                        .is_ok()
                );
                let _ = blocking.send(());
                // Synchronous code run directly in async context never yields, so
                // it holds this worker thread until the test has observed renewal.
                let _ = released.recv_timeout(BLOCKING_WORK_RELEASE_BOUND);
                Err::<ReadyGeneration, _>(PipelineFailure::new(PipelineStage::Resolve))
            })
            .await
    });
    assert!(blocking_started.await.is_ok());
    let before = supervisor.status().await.heartbeat_count();
    tokio::time::sleep(BLOCKING_OBSERVATION_WINDOW).await;
    let during = supervisor.status().await.heartbeat_count();
    let _ = release.send(());
    assert!(
        during >= before.saturating_add(EXPECTED_HEARTBEATS_WHILE_BLOCKED),
        "lease renewal stalled while the root work blocked its thread: {before} -> {during}"
    );
    let result = join(handle).await;
    assert!(
        matches!(
            result,
            Err(SupervisorError::Pipeline {
                stage: PipelineStage::Resolve
            })
        ),
        "unexpected blocked-work result: {result:?}"
    );
    assert_generation_state(&fixture, &generation_id, GenerationState::Failed).await;
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn concurrent_status_readers_never_deadlock_batched_progress() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), boundary_config());
    let runner = supervisor.clone();
    let request_target = target.clone();
    let handle = tokio::spawn(async move {
        runner
            .run(
                request_with_duration(request_target, BOUNDARY_LEASE_DURATION),
                move |context| async move {
                    drop(staged);
                    let progress = context.progress();
                    assert!(progress.begin_stage(PipelineStage::Parse).await.is_ok());
                    let contended_until = tokio::time::Instant::now() + PROGRESS_CONTENTION_WINDOW;
                    while tokio::time::Instant::now() < contended_until {
                        // Like ordered stage reduction, several outputs advance
                        // progress inside one poll before the work yields.
                        for _ in 0..PROGRESS_BATCH_ITEMS {
                            assert!(progress.advance(1, 1).await.is_ok());
                        }
                        tokio::task::yield_now().await;
                    }
                    Err::<ReadyGeneration, _>(PipelineFailure::new(PipelineStage::Parse))
                },
            )
            .await
    });
    // Status readers model `cartograph admin index` polling the job from the CLI.
    let reading = Arc::new(AtomicBool::new(true));
    let readers = (0..STATUS_READERS)
        .map(|_| {
            let supervisor = supervisor.clone();
            let reading = reading.clone();
            tokio::spawn(async move {
                while reading.load(Ordering::Acquire) {
                    let _ = supervisor.status().await;
                    tokio::task::yield_now().await;
                }
            })
        })
        .collect::<Vec<_>>();
    let joined = tokio::time::timeout(PROGRESS_DEADLOCK_BOUND, handle).await;
    reading.store(false, Ordering::Release);
    let result = joined
        .unwrap_or_else(|_| {
            panic!("supervised progress deadlocked behind concurrent status readers")
        })
        .unwrap_or_else(|error| panic!("contended-progress supervisor task failed: {error}"));
    for reader in readers {
        assert!(matches!(
            tokio::time::timeout(STATUS_READER_JOIN_BOUND, reader).await,
            Ok(Ok(()))
        ));
    }
    assert!(
        matches!(
            result,
            Err(SupervisorError::Pipeline {
                stage: PipelineStage::Parse
            })
        ),
        "unexpected contended-progress result: {result:?}"
    );
    assert_generation_state(&fixture, &generation_id, GenerationState::Failed).await;
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn work_finished_during_a_held_heartbeat_still_yields_to_the_work_deadline() {
    let fixture = open_fixture().await;
    install_heartbeat_delay(
        &fixture,
        HELD_HEARTBEAT_DELAY_SECONDS,
        HELD_HEARTBEAT_DELAY_ATTEMPTS,
    )
    .await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), held_heartbeat_config());
    let pool = fixture.pool.clone();
    let schema = fixture.schema.clone();
    let (finished, mut finished_at) = oneshot::channel();
    // The supervisor's budget starts after this instant, so its work deadline is
    // no earlier than this instant plus the work window.
    let window_opened_by = tokio::time::Instant::now();
    let result = supervisor
        .run(
            request_with_duration(target.clone(), HELD_HEARTBEAT_LEASE_DURATION),
            move |context| async move {
                let progress = context.progress();
                progress
                    .begin_stage(PipelineStage::Copy)
                    .await
                    .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
                let ready = context
                    .prepare_generation(GenerationContents::new(
                        staged,
                        canonical(GenerationFacts::default()),
                    ))
                    .await
                    .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
                // Finish only once the held heartbeat is in flight. While this
                // work runs, only the active-work keeper heartbeats; publication
                // and cleanup heartbeats start after the work has ended.
                while !heartbeat_delay_started(&pool, &schema).await {
                    tokio::time::sleep(HELD_HEARTBEAT_POLL_INTERVAL).await;
                    progress
                        .advance(1, 1)
                        .await
                        .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
                }
                let _ = finished.send(tokio::time::Instant::now());
                Ok(ready)
            },
        )
        .await;
    let finished_at = finished_at.try_recv().unwrap_or_else(|_| {
        panic!("work never saw an active-work heartbeat in flight: {result:?}")
    });
    assert!(
        finished_at < window_opened_by + HELD_HEARTBEAT_WORK_WINDOW,
        "work finished {:?} after the run began, too late to race the work deadline",
        finished_at - window_opened_by
    );
    assert!(
        matches!(
            result,
            Err(SupervisorError::Cancelled {
                reason: CancellationReason::OperationDeadline,
                grace_exceeded: false
            })
        ),
        "work finished during a held heartbeat bypassed the work deadline: {result:?}"
    );
    assert_eq!(
        supervisor.status().await.state(),
        SupervisorState::Cancelled
    );
    assert_generation_state(&fixture, &generation_id, GenerationState::Failed).await;
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn work_finished_during_a_held_heartbeat_still_yields_to_a_progress_stall() {
    let fixture = open_fixture().await;
    install_heartbeat_delay(
        &fixture,
        HELD_HEARTBEAT_DELAY_SECONDS,
        HELD_HEARTBEAT_DELAY_ATTEMPTS,
    )
    .await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), stall_race_config());
    let pool = fixture.pool.clone();
    let schema = fixture.schema.clone();
    let (finished, mut finished_signal) = oneshot::channel();
    let result = supervisor
        .run(
            request_with_duration(target.clone(), STALL_RACE_LEASE_DURATION),
            move |context| async move {
                let progress = context.progress();
                progress
                    .begin_stage(PipelineStage::Copy)
                    .await
                    .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
                let ready = context
                    .prepare_generation(GenerationContents::new(
                        staged,
                        canonical(GenerationFacts::default()),
                    ))
                    .await
                    .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
                // Progress continues until the held active-work heartbeat is in
                // flight, then stops; the work finishes at once.
                while !heartbeat_delay_started(&pool, &schema).await {
                    tokio::time::sleep(HELD_HEARTBEAT_POLL_INTERVAL).await;
                    progress
                        .advance(1, 1)
                        .await
                        .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
                }
                let _ = finished.send(());
                Ok(ready)
            },
        )
        .await;
    assert!(
        finished_signal.try_recv().is_ok(),
        "work never saw an active-work heartbeat in flight: {result:?}"
    );
    assert!(
        matches!(
            result,
            Err(SupervisorError::Cancelled {
                reason: CancellationReason::ProgressStalled,
                grace_exceeded: false
            })
        ),
        "work finished during a held heartbeat bypassed the progress watchdog: {result:?}"
    );
    let status = supervisor.status().await;
    assert_eq!(status.state(), SupervisorState::Wedged);
    assert_eq!(
        status.cancellation_reason(),
        Some(CancellationReason::ProgressStalled)
    );
    assert_generation_state(&fixture, &generation_id, GenerationState::Failed).await;
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn children_are_reaped_while_reap_time_lease_renewal_settles() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let child_dropped = Arc::new(AtomicBool::new(false));
    let child_observer = child_dropped.clone();
    let (child_started, child_started_receiver) = oneshot::channel();
    let (root, mut release) = blocked_root();
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), settling_config());
    let runner = supervisor.clone();
    let request_target = target.clone();
    let handle = tokio::spawn(async move {
        runner
            .run(
                request_with_duration(request_target, SETTLING_LEASE_DURATION),
                move |context| async move {
                    drop(staged);
                    assert!(
                        context
                            .progress()
                            .begin_stage(PipelineStage::Resolve)
                            .await
                            .is_ok()
                    );
                    let _child = match context.spawn(1, async move {
                        let _child_drop = DropFlag(child_observer);
                        let _ = child_started.send(());
                        pending::<Result<(), PipelineFailure>>().await
                    }) {
                        Ok(child) => child,
                        Err(error) => panic!("registered child did not spawn: {error}"),
                    };
                    assert!(child_started_receiver.await.is_ok());
                    root.hold();
                    pending::<Result<ReadyGeneration, PipelineFailure>>().await
                },
            )
            .await
    });
    release.started().await;
    assert!(supervisor.cancel());
    wait_for_supervisor_state(&supervisor, SupervisorState::Cancelling).await;
    // Past the grace the run renews the lease while it waits for the root;
    // lock the exact lease row so the next renewal heartbeat is held.
    tokio::time::sleep(BLOCKED_ROOT_SETTLE).await;
    let mut lease_lock = match fixture.pool.begin().await {
        Ok(transaction) => transaction,
        Err(error) => panic!("lease-lock transaction failed: {error}"),
    };
    let lease_lock_statement = format!(
        r#"SELECT lease_id FROM "{}"."project_operation_leases"
            WHERE project_id = CAST($1 AS uuid) AND operation = $2
            FOR UPDATE"#,
        fixture.schema
    );
    if let Err(error) = query(AssertSqlSafe(lease_lock_statement))
        .bind(target.project_id().as_str())
        .bind(target.operation().as_str())
        .fetch_one(&mut *lease_lock)
        .await
    {
        panic!("could not lock exact lease row: {error}");
    }
    wait_for_schema_lock(&fixture.pool, &fixture.schema, "project_operation_leases").await;
    release.release();
    let child_reaped = wait_for_flag(&child_dropped, SETTLING_CHILD_REAP_BOUND).await;
    let renewal_still_held = !handle.is_finished();
    assert!(lease_lock.rollback().await.is_ok());
    let result = join(handle).await;
    assert!(
        renewal_still_held,
        "the run finished while its renewal heartbeat was held: {result:?}"
    );
    assert!(
        child_reaped,
        "registered children waited for reap-time lease renewal to settle: {result:?}"
    );
    assert!(
        matches!(
            result,
            Err(SupervisorError::Cancelled {
                reason: CancellationReason::Requested,
                grace_exceeded: true
            })
        ),
        "unexpected settling-renewal result: {result:?}"
    );
    assert_generation_state(&fixture, &generation_id, GenerationState::Failed).await;
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn cancellation_waits_for_blocked_root_work_and_renews_until_cleanup() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let root_dropped = Arc::new(AtomicBool::new(false));
    let root_observer = root_dropped.clone();
    let (root, mut release) = blocked_root();
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), blocking_work_config());
    let runner = supervisor.clone();
    let request_target = target.clone();
    let handle = tokio::spawn(async move {
        runner
            .run(request(request_target), move |context| async move {
                drop(staged);
                let _root_drop = DropFlag(root_observer);
                assert!(
                    context
                        .progress()
                        .begin_stage(PipelineStage::Resolve)
                        .await
                        .is_ok()
                );
                root.hold();
                pending::<Result<ReadyGeneration, PipelineFailure>>().await
            })
            .await
    });
    release.started().await;
    assert!(supervisor.cancel());
    wait_for_supervisor_state(&supervisor, SupervisorState::Cancelling).await;
    tokio::time::sleep(BLOCKED_ROOT_SETTLE).await;
    let before = supervisor.status().await.heartbeat_count();
    tokio::time::sleep(BLOCKING_OBSERVATION_WINDOW).await;
    let during = supervisor.status().await.heartbeat_count();
    let lease_while_waiting = fixture.database.lease_status(&target).await;
    let returned_early = handle.is_finished();
    release.release();
    let result = join(handle).await;
    assert!(
        !returned_early,
        "cancellation gave up on root work still in a synchronous section: {result:?}"
    );
    assert!(
        during >= before.saturating_add(EXPECTED_HEARTBEATS_WHILE_BLOCKED),
        "the lease was not renewed while cancellation waited for root work: {before} -> {during}"
    );
    assert!(matches!(lease_while_waiting, Ok(Some(_))));
    assert!(
        matches!(
            result,
            Err(SupervisorError::Cancelled {
                reason: CancellationReason::Requested,
                grace_exceeded: true
            })
        ),
        "unexpected blocked-root cancellation result: {result:?}"
    );
    assert!(root_dropped.load(Ordering::Acquire));
    let status = supervisor.status().await;
    assert_eq!(status.state(), SupervisorState::Wedged);
    assert!(status.grace_exceeded());
    assert_generation_state(&fixture, &generation_id, GenerationState::Failed).await;
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn lease_loss_waits_for_blocked_root_work_before_reporting() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let root_dropped = Arc::new(AtomicBool::new(false));
    let root_observer = root_dropped.clone();
    let (root, mut release) = blocked_root();
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), blocking_work_config());
    let runner = supervisor.clone();
    let request_target = target.clone();
    let handle = tokio::spawn(async move {
        runner
            .run(request(request_target), move |context| async move {
                drop(staged);
                let _root_drop = DropFlag(root_observer);
                assert!(
                    context
                        .progress()
                        .begin_stage(PipelineStage::Parse)
                        .await
                        .is_ok()
                );
                root.hold();
                pending::<Result<ReadyGeneration, PipelineFailure>>().await
            })
            .await
    });
    release.started().await;
    expire_lease(&fixture, &target).await;
    let takeover = match fixture
        .database
        .acquire_lease(LeaseRequest::new(
            target.clone(),
            LeaseOwner::new(process::id(), "blocked-root-takeover"),
            TEST_LEASE_DURATION,
        ))
        .await
    {
        Ok(lease) => lease,
        Err(error) => panic!("takeover lease acquisition failed: {error}"),
    };
    wait_for_cancellation_reason(&supervisor, CancellationReason::LeaseLost).await;
    tokio::time::sleep(BLOCKED_ROOT_SETTLE).await;
    let returned_early = handle.is_finished();
    release.release();
    let result = join(handle).await;
    assert!(
        !returned_early,
        "lease loss gave up on root work still in a synchronous section: {result:?}"
    );
    assert!(
        matches!(
            result,
            Err(SupervisorError::Cancelled {
                reason: CancellationReason::LeaseLost,
                grace_exceeded: false
            })
        ),
        "unexpected blocked-root lease-loss result: {result:?}"
    );
    assert!(root_dropped.load(Ordering::Acquire));
    assert_generation_state(&fixture, &generation_id, GenerationState::Staging).await;
    let status = match fixture.database.lease_status(&target).await {
        Ok(Some(status)) => status,
        Ok(None) => panic!("takeover lease disappeared"),
        Err(error) => panic!("takeover lease status failed: {error}"),
    };
    assert_eq!(status.owner_process_start(), "blocked-root-takeover");
    assert!(fixture.database.release_lease(&takeover).await.is_ok());
    fail_recoverable_generation(&fixture, &generation_id).await;

    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn dropping_the_run_while_root_work_is_blocked_still_cleans_up() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let root_dropped = Arc::new(AtomicBool::new(false));
    let root_observer = root_dropped.clone();
    let (root, mut release) = blocked_root();
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), blocking_work_config());
    let runner = supervisor.clone();
    let request_target = target.clone();
    let outer = tokio::spawn(async move {
        runner
            .run(request(request_target), move |context| async move {
                drop(staged);
                let _root_drop = DropFlag(root_observer);
                assert!(
                    context
                        .progress()
                        .begin_stage(PipelineStage::Resolve)
                        .await
                        .is_ok()
                );
                root.hold();
                pending::<Result<ReadyGeneration, PipelineFailure>>().await
            })
            .await
    });
    release.started().await;
    outer.abort();
    assert!(matches!(outer.await, Err(error) if error.is_cancelled()));
    wait_for_supervisor_state(&supervisor, SupervisorState::Cancelling).await;
    tokio::time::sleep(BLOCKED_ROOT_SETTLE).await;
    let while_blocked = supervisor.status().await;
    release.release();
    assert_eq!(
        while_blocked.state(),
        SupervisorState::Cancelling,
        "the dropped run gave up on root work still in a synchronous section"
    );
    wait_for_supervisor_state(&supervisor, SupervisorState::Wedged).await;
    assert!(root_dropped.load(Ordering::Acquire));
    let status = supervisor.status().await;
    assert_eq!(
        status.cancellation_reason(),
        Some(CancellationReason::Requested)
    );
    assert!(status.grace_exceeded());
    assert_generation_state(&fixture, &generation_id, GenerationState::Failed).await;
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn root_work_blocked_past_the_reap_ceiling_is_reported_unreaped() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let (root, mut release) = blocked_root();
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), stalled_config());
    let runner = supervisor.clone();
    let request_target = target.clone();
    // The supervisor's budget starts after this instant, so its reap ceiling is
    // no earlier than this instant plus the ceiling offset.
    let started_by = tokio::time::Instant::now();
    let handle = tokio::spawn(async move {
        runner
            .run(request(request_target), move |context| async move {
                drop(staged);
                assert!(
                    context
                        .progress()
                        .begin_stage(PipelineStage::Resolve)
                        .await
                        .is_ok()
                );
                root.hold();
                pending::<Result<ReadyGeneration, PipelineFailure>>().await
            })
            .await
    });
    release.started().await;
    // The blocked root stalls progress, is cancelled, and is then waited for
    // until the reap ceiling but never past it.
    let joined = tokio::time::timeout(UNREAPED_ROOT_RESULT_BOUND, handle).await;
    let returned_after = started_by.elapsed();
    release.release();
    let result = joined
        .unwrap_or_else(|_| panic!("the run waited past its reap ceiling for blocked root work"))
        .unwrap_or_else(|error| panic!("unreaped-root supervisor task failed: {error}"));
    assert!(
        matches!(result, Err(SupervisorError::UnreapedWorkers)),
        "unexpected unreaped-root result: {result:?}"
    );
    assert!(
        returned_after >= STALLED_REAP_CEILING,
        "the run gave up on its root work after {returned_after:?}, before its reap ceiling"
    );
    assert_eq!(supervisor.status().await.state(), SupervisorState::Failed);
    // Unreaped work forbids owned cleanup; the staging generation is recovered.
    assert_generation_state(&fixture, &generation_id, GenerationState::Staging).await;
    expire_lease(&fixture, &target).await;
    fail_recoverable_generation(&fixture, &generation_id).await;

    fixture.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn cancellation_during_blocked_acquisition_reaps_work_and_leaves_recoverable_staging() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let lock_statement = format!(
        r#"LOCK TABLE "{}"."project_operation_leases" IN ACCESS EXCLUSIVE MODE"#,
        fixture.schema
    );
    let mut lock = match fixture.pool.begin().await {
        Ok(transaction) => transaction,
        Err(error) => panic!("acquisition lock transaction failed: {error}"),
    };
    if let Err(error) = query(AssertSqlSafe(lock_statement))
        .execute(&mut *lock)
        .await
    {
        panic!("acquisition lock failed: {error}");
    }
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), boundary_config());
    let runner = supervisor.clone();
    let work_called = Arc::new(AtomicBool::new(false));
    let work_observer = work_called.clone();
    let request_target = target.clone();
    let handle = tokio::spawn(async move {
        runner
            .run(
                request_with_duration(request_target, BOUNDARY_LEASE_DURATION),
                move |_| async move {
                    work_observer.store(true, Ordering::Release);
                    drop(staged);
                    Err::<ReadyGeneration, _>(PipelineFailure::new(PipelineStage::Discover))
                },
            )
            .await
    });
    wait_for_database_lock(&fixture.pool, &fixture.schema).await;
    assert!(supervisor.cancel());
    let joined = tokio::time::timeout(ABORT_RESULT_BOUND, handle)
        .await
        .unwrap_or_else(|error| {
            panic!("bounded acquisition reconciliation waited for the external blocker: {error}")
        });
    let result = joined
        .unwrap_or_else(|error| panic!("blocked-acquisition supervisor task failed: {error}"));
    // Cancellation may linearize before the exact probe starts (cancelled) or
    // while the access-exclusive lock prevents proof (ambiguous). Both exits
    // must reap every database task before returning.
    assert!(
        matches!(
            result,
            Err(SupervisorError::Cancelled {
                reason: CancellationReason::Requested,
                grace_exceeded: false
            } | SupervisorError::AmbiguousOutcome {
                operation: "acquire"
            })
        ),
        "unexpected blocked acquisition result: {result:?}"
    );
    assert!(!work_called.load(Ordering::Acquire));
    assert_generation_state(&fixture, &generation_id, GenerationState::Staging).await;
    assert_no_active_schema_work(&fixture).await;
    assert_generation_advisories_available(&fixture, &target).await;
    assert!(lock.rollback().await.is_ok());
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));
    assert!(!supervisor.cancel());
    assert_no_active_schema_work(&fixture).await;
    fail_recoverable_generation(&fixture, &generation_id).await;
    assert_generation_state(&fixture, &generation_id, GenerationState::Failed).await;

    fixture.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn timed_out_acquisition_keeps_one_exact_attempt_and_recovers_its_token() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    install_one_shot_acquisition_delay(&fixture).await;
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), reconcile_config());
    let work_called = Arc::new(AtomicBool::new(false));
    let work_observer = work_called.clone();
    let result = supervisor
        .run(request(target.clone()), move |_| async move {
            work_observer.store(true, Ordering::Release);
            drop(staged);
            Err::<ReadyGeneration, _>(PipelineFailure::new(PipelineStage::Discover))
        })
        .await;
    assert!(matches!(
        result,
        Err(SupervisorError::Pipeline {
            stage: PipelineStage::Discover
        })
    ));
    assert!(work_called.load(Ordering::Acquire));
    assert_generation_state(&fixture, &generation_id, GenerationState::Failed).await;
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn publication_gate_rejects_late_cancellation_and_commits_once() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let target = target(&fixture.project, staged.generation_id());
    let publication_key = format!(
        "cartograph-v2-publish:{}:{}",
        fixture.schema, fixture.project
    );
    let mut lock_connection = match fixture.pool.acquire().await {
        Ok(connection) => connection,
        Err(error) => panic!("publication lock connection failed: {error}"),
    };
    if let Err(error) = query("SELECT pg_advisory_lock(hashtextextended($1, 0))")
        .bind(&publication_key)
        .execute(&mut *lock_connection)
        .await
    {
        panic!("publication advisory lock failed: {error}");
    }
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), boundary_config());
    let runner = supervisor.clone();
    let request_target = target.clone();
    let run = runner.run(
        request_with_duration(request_target, BOUNDARY_LEASE_DURATION),
        move |context| async move {
            context
                .prepare_generation(GenerationContents::new(
                    staged,
                    canonical(GenerationFacts::default()),
                ))
                .await
                .map_err(|_| PipelineFailure::new(PipelineStage::Copy))
        },
    );
    let release_publication = async {
        wait_for_supervisor_stage(&supervisor, PipelineStage::Publish).await;
        assert!(!supervisor.cancel());
        if let Err(error) = query("SELECT pg_advisory_unlock(hashtextextended($1, 0))")
            .bind(&publication_key)
            .execute(&mut *lock_connection)
            .await
        {
            panic!("publication advisory unlock failed: {error}");
        }
    };
    let (current, ()) = tokio::join!(run, release_publication);
    let current = match current {
        Ok(current) => current,
        Err(error) => panic!("publication did not finish after gate close: {error}"),
    };
    assert_eq!(current.project_id(), &fixture.project);
    assert_eq!(
        supervisor.status().await.state(),
        SupervisorState::Completed
    );
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));
    assert!(!supervisor.cancel());
    drop(lock_connection);

    fixture.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn timed_out_publication_reconciles_ready_state_retries_and_releases_atomically() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let target = target(&fixture.project, staged.generation_id());
    install_one_shot_publish_delay(&fixture).await;
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), reconcile_config());
    let current = supervisor
        .run(request(target.clone()), move |context| async move {
            context
                .prepare_generation(GenerationContents::new(
                    staged,
                    canonical(GenerationFacts::default()),
                ))
                .await
                .map_err(|_| PipelineFailure::new(PipelineStage::Copy))
        })
        .await;
    let current = match current {
        Ok(current) => current,
        Err(error) => panic!("timed-out publication did not reconcile: {error}"),
    };
    assert_eq!(current.project_id(), &fixture.project);
    assert_eq!(
        supervisor.status().await.state(),
        SupervisorState::Completed
    );
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn timed_out_cleanup_reconciles_failure_and_exact_release_atomically() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    install_one_shot_cleanup_delay(&fixture).await;
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), reconcile_config());
    let runner = supervisor.clone();
    let request_target = target.clone();
    let handle = tokio::spawn(async move {
        runner
            .run(request(request_target), move |context| async move {
                drop(staged);
                let mut cancellation = context.cancellation();
                cancellation.cancelled().await;
                Err::<ReadyGeneration, _>(PipelineFailure::new(PipelineStage::Read))
            })
            .await
    });
    wait_for_lease(&fixture.database, &target).await;
    assert!(supervisor.cancel());
    let result = join(handle).await;
    assert!(matches!(
        result,
        Err(SupervisorError::Cancelled {
            reason: CancellationReason::Requested,
            grace_exceeded: false
        })
    ));
    assert_generation_state(&fixture, &generation_id, GenerationState::Failed).await;
    assert!(matches!(
        fixture.database.lease_status(&target).await,
        Ok(None)
    ));

    fixture.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn blocked_publication_is_aborted_reaped_and_leaves_no_active_query() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let publication_key = format!(
        "cartograph-v2-publish:{}:{}",
        fixture.schema, fixture.project
    );
    let mut lock_connection = match fixture.pool.acquire().await {
        Ok(connection) => connection,
        Err(error) => panic!("publication abort lock connection failed: {error}"),
    };
    if let Err(error) = query("SELECT pg_advisory_lock(hashtextextended($1, 0))")
        .bind(&publication_key)
        .execute(&mut *lock_connection)
        .await
    {
        panic!("publication abort advisory lock failed: {error}");
    }
    // This test must first complete the bounded empty-generation COPY so it can
    // exercise the deliberately blocked publication. Keep that admission
    // independent from the shorter abort-path COPY budget used by other tests.
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), blocked_publication_config());
    let result = tokio::time::timeout(
        BLOCKED_PUBLICATION_RESULT_BOUND,
        supervisor.run(request(target.clone()), move |context| async move {
            context
                .prepare_generation(GenerationContents::new(
                    staged,
                    canonical(GenerationFacts::default()),
                ))
                .await
                .map_err(|_| PipelineFailure::new(PipelineStage::Copy))
        }),
    )
    .await;
    let result = result.unwrap_or_else(|error| {
        panic!("blocked publication exceeded its absolute supervisor deadline: {error}")
    });
    assert!(
        matches!(
            result,
            Err(SupervisorError::AmbiguousOutcome {
                operation: "publish-generation"
            })
        ),
        "unexpected blocked publication result: {result:?}"
    );
    assert_no_active_schema_work(&fixture).await;
    assert_generation_advisories_available(&fixture, &target).await;
    if let Err(error) = query("SELECT pg_advisory_unlock(hashtextextended($1, 0))")
        .bind(&publication_key)
        .execute(&mut *lock_connection)
        .await
    {
        panic!("publication abort advisory unlock failed: {error}");
    }
    drop(lock_connection);
    expire_lease(&fixture, &target).await;
    fail_recoverable_generation(&fixture, &generation_id).await;

    fixture.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn blocked_cleanup_is_aborted_reaped_and_leaves_no_active_query() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let (release_work, work_release) = oneshot::channel();
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), blocked_cleanup_config());
    let run = supervisor.run(request(target.clone()), move |_| async move {
        if work_release.await.is_err() {
            return Err(PipelineFailure::new(PipelineStage::Read));
        }
        drop(staged);
        Err::<ReadyGeneration, _>(PipelineFailure::new(PipelineStage::Read))
    });
    let hold_generation_lock = async {
        wait_for_lease(&fixture.database, &target).await;
        let generation_lock_statement = format!(
            r#"SELECT state FROM "{}"."index_generations"
                WHERE project_id = CAST($1 AS uuid)
                  AND generation_id = CAST($2 AS uuid)
                FOR UPDATE"#,
            fixture.schema
        );
        let mut generation_lock = match fixture.pool.begin().await {
            Ok(transaction) => transaction,
            Err(error) => panic!("cleanup abort lock transaction failed: {error}"),
        };
        if let Err(error) = query(AssertSqlSafe(generation_lock_statement))
            .bind(fixture.project.as_str())
            .bind(generation_id.as_str())
            .fetch_one(&mut *generation_lock)
            .await
        {
            panic!("cleanup abort generation lock failed: {error}");
        }
        assert!(
            release_work.send(()).is_ok(),
            "cleanup abort work release was not observed"
        );
        wait_for_supervisor_state(&supervisor, SupervisorState::Failed).await;
        assert_no_active_schema_work(&fixture).await;
        assert_generation_advisories_available(&fixture, &target).await;
        assert!(generation_lock.rollback().await.is_ok());
    };
    let joined = tokio::time::timeout(ABORT_RESULT_BOUND, async {
        tokio::join!(run, hold_generation_lock)
    })
    .await;
    let (result, ()) = joined.unwrap_or_else(|error| {
        panic!("blocked cleanup exceeded its absolute supervisor deadline: {error}")
    });
    assert!(
        matches!(
            result,
            Err(SupervisorError::AmbiguousOutcome {
                operation: "cleanup-generation"
            })
        ),
        "unexpected blocked cleanup result: {result:?}"
    );
    expire_lease(&fixture, &target).await;
    fail_recoverable_generation(&fixture, &generation_id).await;

    fixture.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn heartbeat_uncertainty_drops_root_and_reaps_registered_children_without_grace() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let root_dropped = Arc::new(AtomicBool::new(false));
    let child_dropped = Arc::new(AtomicBool::new(false));
    let root_observer = root_dropped.clone();
    let child_observer = child_dropped.clone();
    let (child_started, child_started_receiver) = oneshot::channel();
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), uncertain_config());
    let runner = supervisor.clone();
    let request_target = target.clone();
    let handle = tokio::spawn(async move {
        runner
            .run(request(request_target), move |context| async move {
                drop(staged);
                let _root_drop = DropFlag(root_observer);
                let child = match context.spawn(1, async move {
                    let _child_drop = DropFlag(child_observer);
                    let _ = child_started.send(());
                    pending::<Result<(), PipelineFailure>>().await
                }) {
                    Ok(child) => child,
                    Err(error) => panic!("registered child did not spawn: {error}"),
                };
                assert!(child_started_receiver.await.is_ok());
                drop(child);
                pending::<Result<ReadyGeneration, PipelineFailure>>().await
            })
            .await
    });
    wait_for_lease(&fixture.database, &target).await;
    let mut lease_lock = match fixture.pool.begin().await {
        Ok(transaction) => transaction,
        Err(error) => panic!("lease-lock transaction failed: {error}"),
    };
    let lease_lock_statement = format!(
        r#"SELECT lease_id FROM "{}"."project_operation_leases"
            WHERE project_id = CAST($1 AS uuid) AND operation = $2
            FOR UPDATE"#,
        fixture.schema
    );
    if let Err(error) = query(AssertSqlSafe(lease_lock_statement))
        .bind(target.project_id().as_str())
        .bind(target.operation().as_str())
        .fetch_one(&mut *lease_lock)
        .await
    {
        panic!("could not lock exact lease row: {error}");
    }
    let joined = tokio::time::timeout(UNCERTAIN_RESULT_BOUND, handle)
        .await
        .unwrap_or_else(|error| {
            panic!("heartbeat uncertainty incorrectly waited for cancellation grace: {error}")
        });
    let result = joined
        .unwrap_or_else(|error| panic!("uncertain-heartbeat supervisor task failed: {error}"));
    assert!(
        matches!(
            result,
            Err(SupervisorError::Cancelled {
                reason: CancellationReason::LeaseHeartbeatFailed,
                grace_exceeded: false
            })
        ),
        "unexpected uncertain-heartbeat result: {result:?}"
    );
    assert!(root_dropped.load(Ordering::Acquire));
    assert!(child_dropped.load(Ordering::Acquire));
    assert_generation_state(&fixture, &generation_id, GenerationState::Staging).await;
    assert_no_active_schema_work(&fixture).await;
    assert_generation_advisories_available(&fixture, &target).await;
    assert!(lease_lock.rollback().await.is_ok());
    expire_lease(&fixture, &target).await;
    let takeover = match fixture
        .database
        .acquire_lease(LeaseRequest::new(
            target.clone(),
            LeaseOwner::new(process::id(), "uncertain-heartbeat-takeover"),
            TEST_LEASE_DURATION,
        ))
        .await
    {
        Ok(lease) => lease,
        Err(error) => panic!("uncertain-heartbeat takeover failed: {error}"),
    };
    assert!(fixture.database.release_lease(&takeover).await.is_ok());
    fail_recoverable_generation(&fixture, &generation_id).await;

    fixture.close().await;
}

#[tokio::test]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn heartbeat_uncertainty_reaps_concurrent_copy_before_returning() {
    let fixture = open_fixture().await;
    let staged = begin_generation(&fixture).await;
    let generation_id = staged.generation_id().clone();
    let target = target(&fixture.project, &generation_id);
    let table_lock_statement = format!(
        r#"LOCK TABLE "{}"."search_documents" IN ACCESS EXCLUSIVE MODE"#,
        fixture.schema
    );
    let mut table_lock = match fixture.pool.begin().await {
        Ok(transaction) => transaction,
        Err(error) => panic!("combined uncertainty table-lock transaction failed: {error}"),
    };
    if let Err(error) = query(AssertSqlSafe(table_lock_statement))
        .execute(&mut *table_lock)
        .await
    {
        panic!("combined uncertainty table lock failed: {error}");
    }
    let (copy_gate, copy_control) = copy_start_barrier();
    let supervisor = IndexerSupervisor::new(fixture.database.clone(), long_copy_config());
    let runner = supervisor.clone();
    let request_target = target.clone();
    let mut handle = tokio::spawn(async move {
        runner
            .run(request(request_target), move |context| async move {
                context
                    .progress()
                    .begin_stage(PipelineStage::Copy)
                    .await
                    .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
                copy_gate.wait().await?;
                context
                    .prepare_generation(GenerationContents::new(
                        staged,
                        canonical(GenerationFacts {
                            documents: vec![copy_probe_document()],
                            ..GenerationFacts::default()
                        }),
                    ))
                    .await
                    .map_err(|_| PipelineFailure::new(PipelineStage::Copy))
            })
            .await
    });
    wait_for_lease(&fixture.database, &target).await;
    require_combined_copy_lock(copy_control, &supervisor, &fixture, &mut handle).await;
    let mut lease_lock = match fixture.pool.begin().await {
        Ok(transaction) => transaction,
        Err(error) => panic!("combined uncertainty lease-lock transaction failed: {error}"),
    };
    let lease_lock_statement = format!(
        r#"SELECT lease_id FROM "{}"."project_operation_leases"
            WHERE project_id = CAST($1 AS uuid) AND operation = $2
            FOR UPDATE"#,
        fixture.schema
    );
    if let Err(error) = query(AssertSqlSafe(lease_lock_statement))
        .bind(target.project_id().as_str())
        .bind(target.operation().as_str())
        .fetch_one(&mut *lease_lock)
        .await
    {
        panic!("combined uncertainty lease row lock failed: {error}");
    }
    let joined = tokio::time::timeout(ABORT_RESULT_BOUND, handle)
        .await
        .unwrap_or_else(|error| {
            panic!("heartbeat uncertainty did not reap blocked COPY before its bound: {error}")
        });
    let result = joined
        .unwrap_or_else(|error| panic!("combined uncertainty supervisor task failed: {error}"));
    assert!(
        matches!(
            result,
            Err(SupervisorError::Cancelled {
                reason: CancellationReason::LeaseHeartbeatFailed,
                grace_exceeded: false
            })
        ),
        "unexpected combined-uncertainty result: {result:?}"
    );
    assert_no_active_schema_work(&fixture).await;
    assert_generation_advisories_available(&fixture, &target).await;
    assert!(lease_lock.rollback().await.is_ok());
    assert!(table_lock.rollback().await.is_ok());
    assert_generation_state(&fixture, &generation_id, GenerationState::Staging).await;
    expire_lease(&fixture, &target).await;
    fail_recoverable_generation(&fixture, &generation_id).await;

    fixture.close().await;
}

fn standard_config() -> SupervisorConfig {
    SupervisorConfig::new(STANDARD_OPERATION_TIMEOUT)
        .with_heartbeat_interval(STANDARD_HEARTBEAT_INTERVAL)
        .with_heartbeat_timeout(STANDARD_HEARTBEAT_TIMEOUT)
        .with_progress_timeout(STANDARD_PROGRESS_TIMEOUT)
        .with_cancellation_grace(STANDARD_CANCELLATION_GRACE)
        .with_copy_timeout(STANDARD_COPY_TIMEOUT)
}

fn blocking_work_config() -> SupervisorConfig {
    SupervisorConfig::new(BLOCKING_WORK_OPERATION_TIMEOUT)
        .with_heartbeat_interval(STANDARD_HEARTBEAT_INTERVAL)
        .with_heartbeat_timeout(STANDARD_HEARTBEAT_TIMEOUT)
        .with_progress_timeout(BLOCKING_WORK_PROGRESS_TIMEOUT)
        .with_cancellation_grace(STANDARD_CANCELLATION_GRACE)
        .with_copy_timeout(STANDARD_COPY_TIMEOUT)
}

fn held_heartbeat_config() -> SupervisorConfig {
    SupervisorConfig::new(HELD_HEARTBEAT_OPERATION_TIMEOUT)
        .with_heartbeat_interval(HELD_HEARTBEAT_INTERVAL)
        .with_heartbeat_timeout(HELD_HEARTBEAT_TIMEOUT)
        .with_progress_timeout(HELD_HEARTBEAT_PROGRESS_TIMEOUT)
        .with_cancellation_grace(STANDARD_CANCELLATION_GRACE)
        .with_copy_timeout(STANDARD_COPY_TIMEOUT)
}

fn stall_race_config() -> SupervisorConfig {
    SupervisorConfig::new(STALL_RACE_OPERATION_TIMEOUT)
        .with_heartbeat_interval(STALL_RACE_HEARTBEAT_INTERVAL)
        .with_heartbeat_timeout(HELD_HEARTBEAT_TIMEOUT)
        .with_progress_timeout(STALL_RACE_PROGRESS_TIMEOUT)
        .with_cancellation_grace(STANDARD_CANCELLATION_GRACE)
        .with_copy_timeout(STANDARD_COPY_TIMEOUT)
}

fn settling_config() -> SupervisorConfig {
    SupervisorConfig::new(SETTLING_OPERATION_TIMEOUT)
        .with_heartbeat_interval(STANDARD_HEARTBEAT_INTERVAL)
        .with_heartbeat_timeout(SETTLING_HEARTBEAT_TIMEOUT)
        .with_progress_timeout(BLOCKING_WORK_PROGRESS_TIMEOUT)
        .with_cancellation_grace(STANDARD_CANCELLATION_GRACE)
        .with_copy_timeout(STANDARD_COPY_TIMEOUT)
}

fn stalled_config() -> SupervisorConfig {
    standard_config()
        .with_progress_timeout(STALLED_PROGRESS_TIMEOUT)
        .with_cancellation_grace(STANDARD_CANCELLATION_GRACE)
}

fn deadline_config() -> SupervisorConfig {
    SupervisorConfig::new(DEADLINE_TEST_TIMEOUT)
        .with_heartbeat_interval(DEADLINE_HEARTBEAT_INTERVAL)
        .with_heartbeat_timeout(DEADLINE_HEARTBEAT_TIMEOUT)
        .with_progress_timeout(DEADLINE_PROGRESS_TIMEOUT)
        .with_cancellation_grace(DEADLINE_CANCELLATION_GRACE)
        .with_copy_timeout(DEADLINE_COPY_TIMEOUT)
}

fn boundary_config() -> SupervisorConfig {
    SupervisorConfig::new(BOUNDARY_OPERATION_TIMEOUT)
        .with_heartbeat_interval(BOUNDARY_HEARTBEAT_INTERVAL)
        .with_heartbeat_timeout(BOUNDARY_HEARTBEAT_TIMEOUT)
        .with_progress_timeout(BOUNDARY_PROGRESS_TIMEOUT)
        .with_cancellation_grace(BOUNDARY_CANCELLATION_GRACE)
        .with_copy_timeout(BOUNDARY_COPY_TIMEOUT)
}

fn spill_parity_supervisor_config() -> SupervisorConfig {
    SupervisorConfig::new(SPILL_PARITY_OPERATION_TIMEOUT)
        .with_heartbeat_interval(BOUNDARY_HEARTBEAT_INTERVAL)
        .with_heartbeat_timeout(BOUNDARY_HEARTBEAT_TIMEOUT)
        .with_progress_timeout(SPILL_PARITY_PROGRESS_TIMEOUT)
        .with_cancellation_grace(BOUNDARY_CANCELLATION_GRACE)
        .with_copy_timeout(BOUNDARY_COPY_TIMEOUT)
}

fn spill_deadline_supervisor_config() -> SupervisorConfig {
    SupervisorConfig::new(SPILL_PARITY_OPERATION_TIMEOUT)
        .with_heartbeat_interval(BOUNDARY_HEARTBEAT_INTERVAL)
        .with_heartbeat_timeout(BOUNDARY_HEARTBEAT_TIMEOUT)
        .with_progress_timeout(SPILL_PARITY_PROGRESS_TIMEOUT)
        .with_cancellation_grace(BOUNDARY_CANCELLATION_GRACE)
        .with_copy_timeout(Duration::from_secs(3))
}

fn uncertain_config() -> SupervisorConfig {
    SupervisorConfig::new(UNCERTAIN_OPERATION_TIMEOUT)
        .with_heartbeat_interval(UNCERTAIN_HEARTBEAT_INTERVAL)
        .with_heartbeat_timeout(UNCERTAIN_HEARTBEAT_TIMEOUT)
        .with_progress_timeout(UNCERTAIN_PROGRESS_TIMEOUT)
        .with_cancellation_grace(UNCERTAIN_CANCELLATION_GRACE)
        .with_copy_timeout(UNCERTAIN_COPY_TIMEOUT)
}

fn reconcile_config() -> SupervisorConfig {
    SupervisorConfig::new(RECONCILE_OPERATION_TIMEOUT)
        .with_heartbeat_interval(RECONCILE_HEARTBEAT_INTERVAL)
        .with_heartbeat_timeout(RECONCILE_HEARTBEAT_TIMEOUT)
        .with_progress_timeout(RECONCILE_PROGRESS_TIMEOUT)
        .with_cancellation_grace(RECONCILE_CANCELLATION_GRACE)
        .with_copy_timeout(RECONCILE_COPY_TIMEOUT)
}

fn transient_heartbeat_config() -> SupervisorConfig {
    SupervisorConfig::new(TRANSIENT_HEARTBEAT_OPERATION_TIMEOUT)
        .with_heartbeat_interval(TRANSIENT_HEARTBEAT_INTERVAL)
        .with_heartbeat_timeout(TRANSIENT_HEARTBEAT_TIMEOUT)
        .with_progress_timeout(TRANSIENT_HEARTBEAT_PROGRESS_TIMEOUT)
        .with_cancellation_grace(STANDARD_CANCELLATION_GRACE)
        .with_copy_timeout(STANDARD_COPY_TIMEOUT)
}

fn blocked_cleanup_config() -> SupervisorConfig {
    SupervisorConfig::new(ABORT_OPERATION_TIMEOUT)
        .with_heartbeat_interval(ISOLATED_ABORT_HEARTBEAT_INTERVAL)
        .with_heartbeat_timeout(ISOLATED_ABORT_HEARTBEAT_TIMEOUT)
        .with_progress_timeout(BLOCKED_DATABASE_PROGRESS_TIMEOUT)
        .with_cancellation_grace(ABORT_CANCELLATION_GRACE)
        .with_copy_timeout(ABORT_COPY_TIMEOUT)
}

fn blocked_copy_config() -> SupervisorConfig {
    SupervisorConfig::new(ABORT_OPERATION_TIMEOUT)
        .with_heartbeat_interval(ISOLATED_ABORT_HEARTBEAT_INTERVAL)
        .with_heartbeat_timeout(ISOLATED_ABORT_HEARTBEAT_TIMEOUT)
        .with_progress_timeout(BLOCKED_DATABASE_PROGRESS_TIMEOUT)
        .with_cancellation_grace(ABORT_CANCELLATION_GRACE)
        .with_copy_timeout(ABORT_COPY_TIMEOUT)
}

fn blocked_publication_config() -> SupervisorConfig {
    SupervisorConfig::new(BLOCKED_PUBLICATION_OPERATION_TIMEOUT)
        .with_heartbeat_interval(ABORT_HEARTBEAT_INTERVAL)
        .with_heartbeat_timeout(ABORT_HEARTBEAT_TIMEOUT)
        .with_progress_timeout(ABORT_PROGRESS_TIMEOUT)
        .with_cancellation_grace(ABORT_CANCELLATION_GRACE)
        .with_copy_timeout(BLOCKED_PUBLICATION_COPY_TIMEOUT)
}

fn copy_cancel_config() -> SupervisorConfig {
    SupervisorConfig::new(COPY_CANCEL_OPERATION_TIMEOUT)
        .with_heartbeat_interval(ABORT_HEARTBEAT_INTERVAL)
        .with_heartbeat_timeout(COPY_CANCEL_HEARTBEAT_TIMEOUT)
        .with_progress_timeout(ABORT_PROGRESS_TIMEOUT)
        .with_cancellation_grace(COPY_CANCEL_GRACE)
        .with_copy_timeout(COPY_CANCEL_TIMEOUT)
}

fn long_copy_config() -> SupervisorConfig {
    SupervisorConfig::new(LONG_COPY_OPERATION_TIMEOUT)
        .with_heartbeat_interval(ABORT_HEARTBEAT_INTERVAL)
        .with_heartbeat_timeout(ABORT_HEARTBEAT_TIMEOUT)
        .with_progress_timeout(RECONCILE_PROGRESS_TIMEOUT)
        .with_cancellation_grace(ABORT_CANCELLATION_GRACE)
        .with_copy_timeout(LONG_COPY_TIMEOUT)
}

fn large_payload_copy_config() -> SupervisorConfig {
    SupervisorConfig::new(LARGE_COPY_OPERATION_TIMEOUT)
        .with_heartbeat_interval(ABORT_HEARTBEAT_INTERVAL)
        .with_heartbeat_timeout(LARGE_COPY_HEARTBEAT_TIMEOUT)
        .with_progress_timeout(LARGE_COPY_PROGRESS_TIMEOUT)
        .with_cancellation_grace(ABORT_CANCELLATION_GRACE)
        .with_copy_timeout(LARGE_COPY_TIMEOUT)
}

fn request(target: LeaseTarget) -> SupervisorRequest {
    request_with_duration(target, TEST_LEASE_DURATION)
}

fn request_with_duration(target: LeaseTarget, duration: Duration) -> SupervisorRequest {
    SupervisorRequest::new(
        target,
        LeaseOwner::new(process::id(), "supervisor-test-owner"),
        duration,
    )
}

struct DatabaseFixture {
    database: CartographDatabase,
    pool: sqlx_postgres::PgPool,
    schema: String,
    project: ProjectId,
    _schema_guard: TestSchemaGuard,
}

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Root work side of a synchronous section that abort cannot interrupt.
struct BlockedRoot {
    started: oneshot::Sender<()>,
    released: std::sync::mpsc::Receiver<()>,
}

/// Test side of a [`BlockedRoot`] section.
struct RootRelease {
    started: oneshot::Receiver<()>,
    release: std::sync::mpsc::Sender<()>,
}

fn blocked_root() -> (BlockedRoot, RootRelease) {
    let (started, started_receiver) = oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    (
        BlockedRoot { started, released },
        RootRelease {
            started: started_receiver,
            release,
        },
    )
}

impl BlockedRoot {
    /// Report the section started, then block this worker thread under
    /// `block_in_place` until the test releases it or the bound elapses.
    fn hold(self) {
        let _ = self.started.send(());
        tokio::task::block_in_place(|| {
            let _ = self.released.recv_timeout(BLOCKED_ROOT_RELEASE_BOUND);
        });
    }
}

impl RootRelease {
    async fn started(&mut self) {
        assert!(
            (&mut self.started).await.is_ok(),
            "root work never reached its blocked section"
        );
    }

    fn release(&self) {
        let _ = self.release.send(());
    }
}

impl DatabaseFixture {
    async fn close(self) {
        drop(self.database);
        drop_schema(&self.pool, &self.schema).await;
        self.pool.close().await;
    }
}

async fn open_fixture() -> DatabaseFixture {
    let schema = format!(
        "cartograph_supervisor_it_{}_{}",
        process::id(),
        SCHEMA_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    open_fixture_with_schema(&schema).await
}

async fn open_fixture_with_schema(schema: &str) -> DatabaseFixture {
    let database_url = env::var(TEST_DATABASE_URL_ENV).unwrap_or_else(|error| {
        panic!("{TEST_DATABASE_URL_ENV} must be set for the ignored integration test: {error}")
    });
    let settings = DatabaseSettings::parse(&database_url, Some("8"), Some("10000"))
        .and_then(|settings| settings.with_schema(schema));
    let settings = match settings {
        Ok(settings) => settings,
        Err(error) => panic!("supervisor test settings failed validation: {error}"),
    };
    let pool = match cartograph_db::connect(&settings).await {
        Ok(pool) => pool,
        Err(error) => panic!("supervisor test database connection failed: {error}"),
    };
    let database = CartographDatabase::new(pool.clone(), settings.schema().clone());
    if let Err(error) = database.migrate().await {
        panic!("supervisor test migration failed: {error}");
    }
    let project = match database
        .register_project(NewProject::new(
            format!("workspace/supervisor/{schema}"),
            digest(PROJECT_FINGERPRINT),
        ))
        .await
    {
        Ok(project) => project,
        Err(error) => panic!("supervisor test project registration failed: {error}"),
    };
    DatabaseFixture {
        database,
        pool,
        schema: schema.to_owned(),
        project,
        _schema_guard: TestSchemaGuard::new(database_url, schema)
            .unwrap_or_else(|error| panic!("supervisor schema guard failed: {error}")),
    }
}

async fn begin_generation(fixture: &DatabaseFixture) -> cartograph_db::StagedGeneration {
    match fixture
        .database
        .begin_generation(NewGeneration::new(
            fixture.project.clone(),
            REVISION,
            WORKER_COUNT,
        ))
        .await
    {
        Ok(staged) => staged,
        Err(error) => panic!("supervisor fixture generation failed to begin: {error}"),
    }
}

fn target(project: &ProjectId, generation: &GenerationId) -> LeaseTarget {
    LeaseTarget::new(
        project.clone(),
        ProjectOperation::Index,
        Some(generation.clone()),
    )
}

async fn wait_for_lease(database: &CartographDatabase, target: &LeaseTarget) {
    for _ in 0..LEASE_WAIT_ATTEMPTS {
        if matches!(database.lease_status(target).await, Ok(Some(_))) {
            return;
        }
        tokio::time::sleep(LEASE_WAIT_INTERVAL).await;
    }
    panic!("supervisor did not acquire its lease before the test deadline");
}

async fn wait_for_database_lock(pool: &sqlx_postgres::PgPool, schema: &str) {
    wait_for_schema_lock(pool, schema, "project_operation_leases").await;
}

async fn wait_for_schema_lock(pool: &sqlx_postgres::PgPool, schema: &str, relation: &str) {
    let pattern = format!("%{schema}%{relation}%");
    for _ in 0..LEASE_WAIT_ATTEMPTS {
        let row = query(
            r"SELECT EXISTS (
                    SELECT 1 FROM pg_stat_activity
                    WHERE application_name = 'cartograph-v2'
                      AND state = 'active'
                      AND wait_event_type = 'Lock'
                      AND query ILIKE $1
                )",
        )
        .bind(&pattern)
        .fetch_one(pool)
        .await;
        if matches!(row, Ok(row) if row.try_get::<bool, _>(0).unwrap_or(false)) {
            return;
        }
        tokio::time::sleep(LEASE_WAIT_INTERVAL).await;
    }
    panic!("database operation did not reach the expected lock wait");
}

async fn release_copy_and_wait_for_schema_lock(
    control: CopyStartControl,
    supervisor: &IndexerSupervisor,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
) -> Result<(), String> {
    let ready = tokio::time::timeout(ABORT_RESULT_BOUND, control.ready).await;
    match ready {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            return Err(format!(
                "COPY dropped its start barrier before database work; supervisor state: {:?}",
                supervisor.status().await.state()
            ));
        }
        Err(error) => {
            return Err(format!(
                "COPY did not reach its start barrier: {error}; supervisor state: {:?}",
                supervisor.status().await.state()
            ));
        }
    }
    let observer_pool = pool.clone();
    let observer_schema = schema.to_owned();
    let (observer_ready, observer_ready_rx) = oneshot::channel();
    let observer = tokio::spawn(async move {
        wait_for_schema_lock_with_ready_connection(
            &observer_pool,
            &observer_schema,
            "search_documents",
            observer_ready,
        )
        .await;
    });
    tokio::time::timeout(ABORT_RESULT_BOUND, observer_ready_rx)
        .await
        .map_err(|error| format!("COPY lock observer did not acquire a connection: {error}"))?
        .map_err(|_| "COPY lock observer dropped its readiness barrier".to_owned())?;
    control
        .release
        .send(())
        .unwrap_or_else(|()| panic!("COPY dropped its release barrier before database work"));
    observer
        .await
        .map_err(|error| format!("COPY lock observer failed: {error}"))?;
    Ok(())
}

async fn wait_for_schema_lock_with_ready_connection(
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    relation: &str,
    ready: oneshot::Sender<()>,
) {
    let mut connection = pool
        .acquire()
        .await
        .unwrap_or_else(|error| panic!("lock observer connection failed: {error}"));
    ready
        .send(())
        .unwrap_or_else(|()| panic!("lock observer readiness receiver dropped"));
    let pattern = format!("%{schema}%{relation}%");
    for _ in 0..LEASE_WAIT_ATTEMPTS {
        let row = query(
            r"SELECT EXISTS (
                    SELECT 1 FROM pg_stat_activity
                    WHERE application_name = 'cartograph-v2'
                      AND state = 'active'
                      AND wait_event_type = 'Lock'
                      AND query ILIKE $1
                )",
        )
        .bind(&pattern)
        .fetch_one(&mut *connection)
        .await;
        if matches!(row, Ok(row) if row.try_get::<bool, _>(0).unwrap_or(false)) {
            return;
        }
        tokio::time::sleep(LEASE_WAIT_INTERVAL).await;
    }
    panic!("database operation did not reach the expected lock wait");
}

async fn require_combined_copy_lock(
    control: CopyStartControl,
    supervisor: &IndexerSupervisor,
    fixture: &DatabaseFixture,
    handle: &mut tokio::task::JoinHandle<Result<CurrentGeneration, SupervisorError>>,
) {
    if let Err(error) =
        release_copy_and_wait_for_schema_lock(control, supervisor, &fixture.pool, &fixture.schema)
            .await
    {
        let outcome = (&mut *handle).await.unwrap_or_else(|join_error| {
            panic!("combined uncertainty supervisor task failed: {join_error}")
        });
        panic!("{error}; supervisor outcome: {outcome:?}");
    }
}

async fn wait_for_query_absent(pool: &sqlx_postgres::PgPool, schema: &str, query_fragment: &str) {
    let schema_pattern = format!("%{schema}%");
    for _ in 0..LEASE_WAIT_ATTEMPTS {
        let row = query(
            r"SELECT NOT EXISTS (
                    SELECT 1 FROM pg_stat_activity
                    WHERE application_name = 'cartograph-v2'
                      AND state = 'active'
                      AND query ILIKE $1
                      AND query ILIKE $2
                )",
        )
        .bind(&schema_pattern)
        .bind(query_fragment)
        .fetch_one(pool)
        .await;
        if matches!(row, Ok(row) if row.try_get::<bool, _>(0).unwrap_or(false)) {
            return;
        }
        tokio::time::sleep(LEASE_WAIT_INTERVAL).await;
    }
    panic!("supervisor database query remained active after task reaping");
}

async fn assert_no_active_schema_work(fixture: &DatabaseFixture) {
    wait_for_query_absent(&fixture.pool, &fixture.schema, "%").await;
}

async fn assert_generation_advisories_available(fixture: &DatabaseFixture, target: &LeaseTarget) {
    let generation_id = target
        .generation_id()
        .unwrap_or_else(|| panic!("advisory-lock fixture requires a generation-bound target"));
    let operation_key = format!(
        "cartograph-v2-operation:{}:{}:{}",
        fixture.schema,
        target.project_id(),
        target.operation().as_str()
    );
    let generation_key = format!(
        "cartograph-v2-generation:{}:{}:{}",
        fixture.schema,
        target.project_id(),
        generation_id
    );
    let mut connection = match fixture.pool.acquire().await {
        Ok(connection) => connection,
        Err(error) => panic!("advisory-lock probe connection failed: {error}"),
    };
    let acquired = query(
        r"SELECT
                pg_try_advisory_lock(hashtextextended($1, 0)),
                pg_try_advisory_lock(hashtextextended($2, 0))",
    )
    .bind(&operation_key)
    .bind(&generation_key)
    .fetch_one(&mut *connection)
    .await;
    let acquired =
        acquired.and_then(|row| Ok((row.try_get::<bool, _>(0)?, row.try_get::<bool, _>(1)?)));
    let released = query(
        r"SELECT
                pg_advisory_unlock(hashtextextended($1, 0)),
                pg_advisory_unlock(hashtextextended($2, 0))",
    )
    .bind(&operation_key)
    .bind(&generation_key)
    .execute(&mut *connection)
    .await;
    assert!(released.is_ok());
    assert!(matches!(acquired, Ok((true, true))));
}

async fn wait_for_supervisor_stage(supervisor: &IndexerSupervisor, expected: PipelineStage) {
    for _ in 0..INSTRUMENTED_STAGE_WAIT_ATTEMPTS {
        if supervisor.status().await.stage() == Some(expected) {
            return;
        }
        tokio::time::sleep(LEASE_WAIT_INTERVAL).await;
    }
    panic!(
        "supervisor did not reach {expected:?}; final status: {:?}",
        supervisor.status().await
    );
}

async fn wait_for_supervisor_state(supervisor: &IndexerSupervisor, expected: SupervisorState) {
    for _ in 0..LEASE_WAIT_ATTEMPTS {
        if supervisor.status().await.state() == expected {
            return;
        }
        tokio::time::sleep(LEASE_WAIT_INTERVAL).await;
    }
    let actual = supervisor.status().await.state();
    assert_eq!(
        actual, expected,
        "supervisor did not reach its expected terminal state"
    );
}

/// Wait up to `bound` for `flag` to be set; true once it is.
async fn wait_for_flag(flag: &AtomicBool, bound: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + bound;
    while !flag.load(Ordering::Acquire) {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(LEASE_WAIT_INTERVAL).await;
    }
    true
}

async fn wait_for_cancellation_reason(
    supervisor: &IndexerSupervisor,
    expected: CancellationReason,
) {
    for _ in 0..LEASE_WAIT_ATTEMPTS {
        if supervisor.status().await.cancellation_reason() == Some(expected) {
            return;
        }
        tokio::time::sleep(LEASE_WAIT_INTERVAL).await;
    }
    panic!(
        "supervisor did not select {expected:?}; final status: {:?}",
        supervisor.status().await
    );
}

async fn install_one_shot_publish_delay(fixture: &DatabaseFixture) {
    let sequence = format!(
        r#"CREATE SEQUENCE "{}"."publish_delay_sequence""#,
        fixture.schema
    );
    let function = format!(
        r#"CREATE FUNCTION "{}"."delay_first_publish"()
            RETURNS trigger
            LANGUAGE plpgsql
            AS $delay$
            BEGIN
                IF nextval('"{}"."publish_delay_sequence"'::regclass) = 1 THEN
                    PERFORM pg_sleep({FIRST_MUTATION_DELAY_SECONDS});
                END IF;
                RETURN NEW;
            END
            $delay$"#,
        fixture.schema, fixture.schema
    );
    let trigger = format!(
        r#"CREATE TRIGGER delay_first_publish
            BEFORE UPDATE OF current_generation_id
            ON "{}"."projects"
            FOR EACH ROW EXECUTE FUNCTION "{}"."delay_first_publish"()"#,
        fixture.schema, fixture.schema
    );
    for statement in [sequence, function, trigger] {
        if let Err(error) = query(AssertSqlSafe(statement)).execute(&fixture.pool).await {
            panic!("could not install one-shot publication delay: {error}");
        }
    }
}

async fn install_one_shot_acquisition_delay(fixture: &DatabaseFixture) {
    let sequence = format!(
        r#"CREATE SEQUENCE "{}"."acquisition_delay_sequence""#,
        fixture.schema
    );
    let function = format!(
        r#"CREATE FUNCTION "{}"."delay_first_acquisition"()
            RETURNS trigger
            LANGUAGE plpgsql
            AS $delay$
            BEGIN
                IF nextval('"{}"."acquisition_delay_sequence"'::regclass) = 1 THEN
                    PERFORM pg_sleep({FIRST_MUTATION_DELAY_SECONDS});
                END IF;
                RETURN NEW;
            END
            $delay$"#,
        fixture.schema, fixture.schema
    );
    let trigger = format!(
        r#"CREATE TRIGGER delay_first_acquisition
            BEFORE INSERT
            ON "{}"."project_operation_leases"
            FOR EACH ROW EXECUTE FUNCTION "{}"."delay_first_acquisition"()"#,
        fixture.schema, fixture.schema
    );
    for statement in [sequence, function, trigger] {
        if let Err(error) = query(AssertSqlSafe(statement)).execute(&fixture.pool).await {
            panic!("could not install one-shot acquisition delay: {error}");
        }
    }
}

async fn install_one_shot_heartbeat_delay(fixture: &DatabaseFixture) {
    install_heartbeat_delay(
        fixture,
        TRANSIENT_HEARTBEAT_DELAY_SECONDS,
        TRANSIENT_HEARTBEAT_DELAY_ATTEMPTS,
    )
    .await;
}

/// Delay the first `attempts` lease heartbeats by `seconds` inside PostgreSQL.
async fn install_heartbeat_delay(fixture: &DatabaseFixture, seconds: &str, attempts: i64) {
    let sequence = format!(
        r#"CREATE SEQUENCE "{}"."heartbeat_delay_sequence""#,
        fixture.schema
    );
    let function = format!(
        r#"CREATE FUNCTION "{}"."delay_first_heartbeat"()
            RETURNS trigger
            LANGUAGE plpgsql
            AS $delay$
            BEGIN
                IF nextval('"{}"."heartbeat_delay_sequence"'::regclass)
                   <= {attempts} THEN
                    PERFORM pg_sleep({seconds});
                END IF;
                RETURN NEW;
            END
            $delay$"#,
        fixture.schema, fixture.schema
    );
    let trigger = format!(
        r#"CREATE TRIGGER delay_first_heartbeat
            BEFORE UPDATE OF heartbeat_at
            ON "{}"."project_operation_leases"
            FOR EACH ROW EXECUTE FUNCTION "{}"."delay_first_heartbeat"()"#,
        fixture.schema, fixture.schema
    );
    for statement in [sequence, function, trigger] {
        if let Err(error) = query(AssertSqlSafe(statement)).execute(&fixture.pool).await {
            panic!("could not install one-shot heartbeat delay: {error}");
        }
    }
}

/// Whether a delayed heartbeat has started executing its delay trigger.
async fn heartbeat_delay_started(pool: &sqlx_postgres::PgPool, schema: &str) -> bool {
    let statement = format!(r#"SELECT is_called FROM "{schema}"."heartbeat_delay_sequence""#);
    query(AssertSqlSafe(statement))
        .fetch_one(pool)
        .await
        .ok()
        .and_then(|row| row.try_get::<bool, _>(0).ok())
        .unwrap_or(false)
}

async fn heartbeat_delay_attempts(fixture: &DatabaseFixture) -> i64 {
    let statement = format!(
        r#"SELECT last_value::bigint FROM "{}"."heartbeat_delay_sequence""#,
        fixture.schema
    );
    match query(AssertSqlSafe(statement))
        .fetch_one(&fixture.pool)
        .await
    {
        Ok(row) => row
            .try_get::<i64, _>(0)
            .unwrap_or_else(|error| panic!("heartbeat delay sequence was invalid: {error}")),
        Err(error) => panic!("heartbeat delay attempts were unavailable: {error}"),
    }
}

async fn install_one_shot_cleanup_delay(fixture: &DatabaseFixture) {
    let sequence = format!(
        r#"CREATE SEQUENCE "{}"."cleanup_delay_sequence""#,
        fixture.schema
    );
    let function = format!(
        r#"CREATE FUNCTION "{}"."delay_first_cleanup"()
            RETURNS trigger
            LANGUAGE plpgsql
            AS $delay$
            BEGIN
                IF NEW.state = 'failed'
                   AND nextval('"{}"."cleanup_delay_sequence"'::regclass) = 1 THEN
                    PERFORM pg_sleep({FIRST_MUTATION_DELAY_SECONDS});
                END IF;
                RETURN NEW;
            END
            $delay$"#,
        fixture.schema, fixture.schema
    );
    let trigger = format!(
        r#"CREATE TRIGGER delay_first_cleanup
            BEFORE UPDATE OF state
            ON "{}"."index_generations"
            FOR EACH ROW EXECUTE FUNCTION "{}"."delay_first_cleanup"()"#,
        fixture.schema, fixture.schema
    );
    for statement in [sequence, function, trigger] {
        if let Err(error) = query(AssertSqlSafe(statement)).execute(&fixture.pool).await {
            panic!("could not install one-shot cleanup delay: {error}");
        }
    }
}

async fn install_copy_delay(fixture: &DatabaseFixture) {
    let function = format!(
        r#"CREATE FUNCTION "{}"."delay_search_document_copy"()
            RETURNS trigger
            LANGUAGE plpgsql
            AS $delay$
            BEGIN
                PERFORM pg_sleep({LARGE_COPY_TRIGGER_DELAY_SECONDS});
                RETURN NULL;
            END
            $delay$"#,
        fixture.schema
    );
    let trigger = format!(
        r#"CREATE TRIGGER delay_search_document_copy
            AFTER INSERT ON "{}"."search_documents"
            FOR EACH STATEMENT
            EXECUTE FUNCTION "{}"."delay_search_document_copy"()"#,
        fixture.schema, fixture.schema
    );
    for statement in [function, trigger] {
        if let Err(error) = query(AssertSqlSafe(statement)).execute(&fixture.pool).await {
            panic!("could not install long-COPY delay: {error}");
        }
    }
}

async fn expire_lease(fixture: &DatabaseFixture, target: &LeaseTarget) {
    let statement = format!(
        r#"UPDATE "{}"."project_operation_leases"
            SET acquired_at = clock_timestamp() - interval '3 seconds',
                heartbeat_at = clock_timestamp() - interval '2 seconds',
                expires_at = clock_timestamp() - interval '1 second'
            WHERE project_id = CAST($1 AS uuid) AND operation = $2"#,
        fixture.schema,
    );
    if let Err(error) = query(AssertSqlSafe(statement))
        .bind(target.project_id().as_str())
        .bind(target.operation().as_str())
        .execute(&fixture.pool)
        .await
    {
        panic!("could not expire supervisor lease fixture: {error}");
    }
}

async fn assert_generation_state(
    fixture: &DatabaseFixture,
    generation: &GenerationId,
    expected: GenerationState,
) {
    for _ in 0..LEASE_WAIT_ATTEMPTS {
        if matches!(
            fixture
                .database
                .generation_state(&fixture.project, generation)
                .await,
            Ok(Some(state)) if state == expected
        ) {
            return;
        }
        tokio::time::sleep(LEASE_WAIT_INTERVAL).await;
    }
    let actual = fixture
        .database
        .generation_state(&fixture.project, generation)
        .await;
    assert!(
        matches!(&actual, Ok(Some(state)) if *state == expected),
        "generation did not reach {expected:?}: {actual:?}"
    );
}

async fn fail_recoverable_generation(fixture: &DatabaseFixture, generation: &GenerationId) {
    let recovered = match fixture
        .database
        .recover_generation(GenerationRecoveryRequest::new(&fixture.project, generation))
        .await
    {
        Ok(Some(recovered)) => recovered,
        Ok(None) => panic!("staging generation was not recoverable after lease takeover"),
        Err(error) => panic!("generation recovery after lease takeover failed: {error}"),
    };
    let lease = match fixture
        .database
        .acquire_lease(LeaseRequest::new(
            target(&fixture.project, generation),
            LeaseOwner::new(process::id(), "supervisor-cleanup-owner"),
            TEST_LEASE_DURATION,
        ))
        .await
    {
        Ok(lease) => lease,
        Err(error) => panic!("cleanup lease acquisition failed: {error}"),
    };
    assert!(
        fixture
            .database
            .fail_generation(recovered, &lease.fence())
            .await
            .is_ok()
    );
    assert!(fixture.database.release_lease(&lease).await.is_ok());
}

async fn join<T>(handle: tokio::task::JoinHandle<T>) -> T {
    match handle.await {
        Ok(result) => result,
        Err(error) => panic!("supervisor task did not join cleanly: {error}"),
    }
}

async fn drop_schema(pool: &sqlx_postgres::PgPool, schema: &str) {
    let statement = format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE");
    if let Err(error) = query(AssertSqlSafe(statement)).execute(pool).await {
        panic!("failed to drop isolated supervisor schema: {error}");
    }
}

fn digest(raw: &str) -> ContentDigest {
    match ContentDigest::parse(raw) {
        Ok(digest) => digest,
        Err(error) => panic!("fixture digest is invalid: {error}"),
    }
}

fn copy_probe_document() -> SearchDocumentInput {
    let document_id = match DocumentId::parse(COPY_PROBE_DOCUMENT) {
        Ok(document_id) => document_id,
        Err(error) => panic!("COPY probe document ID is invalid: {error}"),
    };
    SearchDocumentInput {
        document_id,
        file_id: None,
        symbol_id: None,
        path: "src/supervised_copy.rs".to_owned(),
        language: "rust".to_owned(),
        kind: DocumentKind::Symbol,
        qualified_name: "supervised_copy_probe".to_owned(),
        code: "fn supervised_copy_probe() {}".to_owned(),
        natural_text: "supervised COPY cancellation probe".to_owned(),
        metadata: serde_json::json!({}),
    }
}

fn canonical(facts: GenerationFacts) -> CanonicalGenerationFacts {
    let limits = GenerationValidationLimits::new(
        NATIVE_MAX_GENERATION_BYTES,
        NATIVE_MAX_GENERATION_BYTES.saturating_mul(4),
    )
    .unwrap_or_else(|error| panic!("supervisor validation limits were invalid: {error}"));
    validate_generation_facts(facts, limits, || false).map_or_else(
        |error| panic!("supervisor fixture was invalid: {error}"),
        |(facts, _)| facts,
    )
}

fn large_copy_probe_document() -> SearchDocumentInput {
    let mut document = copy_probe_document();
    document.code = "x".repeat(LARGE_COPY_CODE_BYTES);
    document.natural_text.clear();
    document
        .natural_text
        .push_str("large payload COPY deadline probe");
    document
}
