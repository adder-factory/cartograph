//! The scaling benchmark's deterministic fixture and planning contracts.

#[path = "../test_support/dependency_ownership.rs"]
mod dependency_ownership;
#[path = "../benches/index_scaling/fixture.rs"]
mod fixture;

use std::time::Duration;

use cartograph_indexer::SupervisorConfig;
use fixture::{
    BENCHMARK_CONFIG, BenchmarkDeadlines, EXPECTED_FIXTURE_FINGERPRINT, EXPECTED_SOURCE_DIGEST,
    FrozenFixture, SamplePlan, ValidationBudget, WorkloadSize, fixture_fingerprint,
};

#[test]
fn shared_configuration_preserves_the_frozen_fixture() {
    let fixture = FrozenFixture::build(&BENCHMARK_CONFIG)
        .unwrap_or_else(|error| panic!("frozen fixture must remain valid: {error}"));
    assert_eq!(fixture.source_digest, EXPECTED_SOURCE_DIGEST);
    assert_eq!(fixture.fixture_fingerprint, EXPECTED_FIXTURE_FINGERPRINT);
    assert_eq!(fixture.inputs.len(), 256);
    for input in &fixture.inputs {
        assert_eq!(input.fixture_name, "synthetic-typescript-stage-v1");
        assert_eq!(input.source_repetitions, 192);
        assert_eq!(input.hash_rounds, 32);
        assert_eq!(input.source.lines().count(), 192);
    }
    assert_eq!(
        fixture.config.workload[WorkloadSize::WarmupSamples as usize],
        1
    );
    assert_eq!(
        fixture.config.workload[WorkloadSize::MeasuredSamples as usize],
        5
    );
    assert_eq!(
        fixture.config.validation_bytes[ValidationBudget::Output as usize],
        64 * 1024 * 1024
    );
    assert_eq!(
        fixture.source_bytes,
        fixture
            .inputs
            .iter()
            .map(|input| {
                u64::try_from(input.source.len())
                    .unwrap_or_else(|error| panic!("fixture source length must fit u64: {error}"))
            })
            .sum::<u64>()
    );
    assert_eq!(fixture.expected_rows.files, 256);
    assert_eq!(fixture.expected_rows.symbols, 256);
    assert_eq!(fixture.expected_rows.edges, 255);
    assert_eq!(fixture.expected_rows.references, 255);
    assert_eq!(fixture.expected_rows.numerical_sites, 0);
    assert_eq!(fixture.expected_rows.documents, 256);
    assert_eq!(
        fixture.needle_document_id.as_str(),
        "30000000-0000-4000-8000-000000000001"
    );
}

#[test]
fn every_configuration_value_participates_in_the_fingerprint() {
    let fixture = FrozenFixture::build(&BENCHMARK_CONFIG)
        .unwrap_or_else(|error| panic!("frozen fixture must remain valid: {error}"));
    let mut renamed = BENCHMARK_CONFIG;
    renamed.fixture_name = "changed fixture";
    let mut revised = BENCHMARK_CONFIG;
    revised.source_revision = "changed revision";
    let mut queried = BENCHMARK_CONFIG;
    queried.bm25_query = "changed query";
    let mut configurations = vec![renamed, revised, queried];
    for slot in 0..BENCHMARK_CONFIG.workload.len() {
        let mut config = BENCHMARK_CONFIG;
        config.workload[slot] += 1;
        configurations.push(config);
    }
    let timing_changes: [fn(&mut BenchmarkDeadlines); 9] = [
        |deadlines| deadlines.operation += Duration::from_nanos(1),
        |deadlines| deadlines.stage += Duration::from_nanos(1),
        |deadlines| deadlines.item += Duration::from_nanos(1),
        |deadlines| deadlines.copy += Duration::from_nanos(1),
        |deadlines| deadlines.progress += Duration::from_nanos(1),
        |deadlines| deadlines.heartbeat_interval += Duration::from_nanos(1),
        |deadlines| deadlines.heartbeat_timeout += Duration::from_nanos(1),
        |deadlines| deadlines.cleanup_grace += Duration::from_nanos(1),
        |deadlines| deadlines.lease += Duration::from_nanos(1),
    ];
    for change in timing_changes {
        let mut config = BENCHMARK_CONFIG;
        change(&mut config.deadlines);
        configurations.push(config);
    }
    for slot in 0..BENCHMARK_CONFIG.validation_bytes.len() {
        let mut config = BENCHMARK_CONFIG;
        config.validation_bytes[slot] += 1;
        configurations.push(config);
    }
    for slot in 0..BENCHMARK_CONFIG.workers.len() {
        let mut config = BENCHMARK_CONFIG;
        config.workers[slot] += 1;
        configurations.push(config);
    }
    for config in configurations {
        let actual = fixture_fingerprint(&config, &fixture.inputs)
            .unwrap_or_else(|error| panic!("configuration fingerprint failed: {error}"));
        assert_ne!(actual, EXPECTED_FIXTURE_FINGERPRINT);
    }
}

#[test]
fn sample_planning_consumes_the_fixture_configuration() {
    let mut fixture = FrozenFixture::build(&BENCHMARK_CONFIG)
        .unwrap_or_else(|error| panic!("frozen fixture must remain valid: {error}"));
    fixture.config.deadlines.stage = Duration::from_secs(7);
    fixture.config.validation_bytes[ValidationBudget::Working as usize] = 8 * 1024 * 1024;
    let before = tokio::time::Instant::now();
    let plan = SamplePlan::build(&fixture, 2)
        .unwrap_or_else(|error| panic!("configured sample plan failed: {error}"));
    let after = tokio::time::Instant::now();
    assert!(plan.stage_deadline >= before + Duration::from_secs(7));
    assert!(plan.stage_deadline <= after + Duration::from_secs(7));
    assert_eq!(plan.maximum_reserved_bytes, 8 * 1024 * 1024);
    assert_eq!(plan.inputs.len(), 256);
    assert_eq!(plan.worker_count, 2);
    assert_eq!(plan.queue_items, 2);
    assert_eq!(plan.window, 4);
}

#[test]
fn default_timing_values_and_supervisor_policy_match_the_committed_benchmark() {
    let deadlines = BENCHMARK_CONFIG.deadlines;
    assert_eq!(deadlines.operation, Duration::from_secs(60));
    assert_eq!(deadlines.stage, Duration::from_secs(30));
    assert_eq!(deadlines.item, Duration::from_secs(25));
    assert_eq!(deadlines.copy, Duration::from_secs(20));
    assert_eq!(deadlines.progress, Duration::from_secs(20));
    assert_eq!(deadlines.heartbeat_interval, Duration::from_secs(1));
    assert_eq!(deadlines.heartbeat_timeout, Duration::from_millis(500));
    assert_eq!(deadlines.cleanup_grace, Duration::from_secs(5));
    assert_eq!(deadlines.lease, Duration::from_secs(60));
    let expected = SupervisorConfig::new(Duration::from_secs(60))
        .with_heartbeat_interval(Duration::from_secs(1))
        .with_heartbeat_timeout(Duration::from_millis(500))
        .with_progress_timeout(Duration::from_secs(20))
        .with_cancellation_grace(Duration::from_secs(5))
        .with_copy_timeout(Duration::from_secs(20))
        .with_max_worker_tasks(4)
        .with_max_worker_bytes(8 * 1024 * 1024);
    assert_eq!(
        BENCHMARK_CONFIG.supervisor_config(4, 8 * 1024 * 1024),
        expected
    );
}
