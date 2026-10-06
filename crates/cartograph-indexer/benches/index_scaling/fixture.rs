//! Shared, deterministic fixture construction and planning for the scaling benchmark.

use std::time::Duration;

use cartograph_domain::DocumentId;
use cartograph_indexer::{
    StageEnvelope, StageItemBudget, StageItemMeta, StageSequence, SupervisorConfig,
};
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum FixtureError {
    #[error("index scaling benchmark invariant failed: {0}")]
    Invariant(&'static str),
    #[error("index scaling benchmark fixture fingerprint changed to {0}")]
    FingerprintChanged(String),
}

type FixtureResult<T> = Result<T, FixtureError>;

#[derive(Clone, Copy)]
pub(crate) enum WorkloadSize {
    Items = 0,
    SourceRepetitions = 1,
    HashRounds = 2,
    WarmupSamples = 3,
    MeasuredSamples = 4,
}

#[derive(Clone, Copy)]
pub(crate) struct BenchmarkDeadlines {
    pub(crate) operation: Duration,
    pub(crate) stage: Duration,
    pub(crate) item: Duration,
    pub(crate) copy: Duration,
    pub(crate) progress: Duration,
    pub(crate) heartbeat_interval: Duration,
    pub(crate) heartbeat_timeout: Duration,
    pub(crate) cleanup_grace: Duration,
    pub(crate) lease: Duration,
}

impl BenchmarkDeadlines {
    // This order is part of the frozen fingerprint encoding; execution uses
    // the named fields independently of their declaration order.
    const fn fingerprint_values(self) -> [Duration; 9] {
        [
            self.operation,
            self.stage,
            self.item,
            self.copy,
            self.progress,
            self.heartbeat_interval,
            self.heartbeat_timeout,
            self.cleanup_grace,
            self.lease,
        ]
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ValidationBudget {
    Output = 0,
    Working = 1,
}

#[derive(Clone, Copy)]
pub(crate) struct BenchmarkConfig {
    pub(crate) fixture_name: &'static str,
    pub(crate) source_revision: &'static str,
    pub(crate) bm25_query: &'static str,
    pub(crate) workload: [usize; 5],
    pub(crate) deadlines: BenchmarkDeadlines,
    pub(crate) validation_bytes: [u64; 2],
    pub(crate) workers: [u16; 5],
}

pub(crate) const BENCHMARK_CONFIG: BenchmarkConfig = BenchmarkConfig {
    fixture_name: "synthetic-typescript-stage-v1",
    source_revision: "9999999999999999999999999999999999999999",
    bm25_query: "needle cartograph benchmark",
    workload: [256, 192, 32, 1, 5],
    deadlines: BenchmarkDeadlines {
        operation: Duration::from_mins(1),
        stage: Duration::from_secs(30),
        item: Duration::from_secs(25),
        copy: Duration::from_secs(20),
        progress: Duration::from_secs(20),
        heartbeat_interval: Duration::from_secs(1),
        heartbeat_timeout: Duration::from_millis(500),
        cleanup_grace: Duration::from_secs(5),
        lease: Duration::from_mins(1),
    },
    validation_bytes: [64 * 1024 * 1024, 256 * 1024 * 1024],
    workers: [1, 2, 4, 8, 16],
};

pub(crate) const EXPECTED_SOURCE_DIGEST: &str =
    "b23964be1dfad94c41d158358db1f60187729c399ed623d107c6b4cc0f46d6d1";
pub(crate) const EXPECTED_FIXTURE_FINGERPRINT: &str =
    "2c02e8357bee04c11d89f383c316077b8eb2228bd4262d2404cb1535885083d9";
pub(crate) const EXPECTED_BM25_DOCUMENT_ID: &str = "30000000-0000-4000-8000-000000000001";
const EXPECTED_FILES: i64 = 256;
const EXPECTED_SYMBOLS: i64 = 256;
const EXPECTED_EDGES: i64 = 255;
const EXPECTED_REFERENCES: i64 = 255;
const EXPECTED_NUMERICAL_SITES: i64 = 0;
const EXPECTED_DOCUMENTS: i64 = 256;

#[derive(Clone)]
pub(crate) struct FixtureInput {
    pub(crate) index: usize,
    pub(crate) source: String,
    pub(crate) fixture_name: &'static str,
    pub(crate) source_repetitions: usize,
    pub(crate) hash_rounds: usize,
}

pub(crate) struct FrozenFixture {
    pub(crate) config: BenchmarkConfig,
    pub(crate) inputs: Vec<FixtureInput>,
    pub(crate) source_bytes: u64,
    pub(crate) source_digest: String,
    pub(crate) fixture_fingerprint: String,
    pub(crate) maximum_item_bytes: u64,
    pub(crate) expected_rows: RowCounts,
    pub(crate) needle_document_id: DocumentId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct RowCounts {
    pub(crate) files: i64,
    pub(crate) symbols: i64,
    pub(crate) edges: i64,
    pub(crate) references: i64,
    pub(crate) numerical_sites: i64,
    pub(crate) documents: i64,
}

pub(crate) struct SamplePlan {
    pub(crate) worker_count: usize,
    pub(crate) queue_items: usize,
    pub(crate) window: usize,
    pub(crate) maximum_reserved_bytes: u64,
    pub(crate) stage_deadline: tokio::time::Instant,
    pub(crate) inputs: Vec<StageEnvelope<String, FixtureInput>>,
}

impl FrozenFixture {
    pub(crate) fn build(config: &BenchmarkConfig) -> FixtureResult<Self> {
        let mut inputs = Vec::with_capacity(config.workload[WorkloadSize::Items as usize]);
        let mut source_bytes = 0_u64;
        let mut maximum_item_bytes = 0_u64;
        let mut source_hasher = blake3::Hasher::new();
        for index in 0..config.workload[WorkloadSize::Items as usize] {
            let source = fixture_source(
                index,
                config.workload[WorkloadSize::SourceRepetitions as usize],
            );
            let bytes = u64::try_from(source.len())
                .map_err(|_| FixtureError::Invariant("source-byte-size"))?;
            source_bytes = source_bytes
                .checked_add(bytes)
                .ok_or(FixtureError::Invariant("total-source-byte-size"))?;
            maximum_item_bytes = maximum_item_bytes.max(bytes);
            source_hasher.update(source.as_bytes());
            inputs.push(FixtureInput {
                index,
                source,
                fixture_name: config.fixture_name,
                source_repetitions: config.workload[WorkloadSize::SourceRepetitions as usize],
                hash_rounds: config.workload[WorkloadSize::HashRounds as usize],
            });
        }
        let source_digest = source_hasher.finalize().to_hex().to_string();
        let fixture_fingerprint = fixture_fingerprint(config, &inputs)?;
        if source_digest != EXPECTED_SOURCE_DIGEST {
            return Err(FixtureError::Invariant("committed-source-digest"));
        }
        if fixture_fingerprint != EXPECTED_FIXTURE_FINGERPRINT {
            return Err(FixtureError::FingerprintChanged(fixture_fingerprint));
        }
        Ok(Self {
            config: *config,
            inputs,
            source_bytes,
            source_digest,
            fixture_fingerprint,
            maximum_item_bytes,
            expected_rows: committed_row_counts(),
            needle_document_id: DocumentId::parse(EXPECTED_BM25_DOCUMENT_ID)
                .map_err(|_| FixtureError::Invariant("committed-bm25-document-id"))?,
        })
    }

    fn envelopes(
        &self,
        deadline: tokio::time::Instant,
    ) -> FixtureResult<Vec<StageEnvelope<String, FixtureInput>>> {
        self.inputs
            .iter()
            .cloned()
            .enumerate()
            .map(|(sequence, input)| {
                let reserved_bytes = u64::try_from(input.source.len())
                    .map_err(|_| FixtureError::Invariant("item-reserved-bytes"))?;
                let sequence = u64::try_from(sequence)
                    .map_err(|_| FixtureError::Invariant("stage-sequence-representation"))?;
                let path = normalized_path(input.index);
                Ok(StageEnvelope::new(
                    StageItemMeta::new(
                        StageSequence::new(sequence),
                        path,
                        StageItemBudget::new(reserved_bytes, reserved_bytes, deadline),
                    ),
                    input,
                ))
            })
            .collect()
    }
}

impl SamplePlan {
    pub(crate) fn build(fixture: &FrozenFixture, workers: u16) -> FixtureResult<Self> {
        let worker_count = usize::from(workers);
        let queue_items = worker_count;
        let window = worker_count
            .checked_add(queue_items)
            .ok_or(FixtureError::Invariant("bounded-window"))?;
        let maximum_reserved_bytes = maximum_stage_reserved_bytes(fixture, window)?;
        let now = tokio::time::Instant::now();
        Ok(Self {
            worker_count,
            queue_items,
            window,
            maximum_reserved_bytes,
            stage_deadline: now + fixture.config.deadlines.stage,
            inputs: fixture.envelopes(now + fixture.config.deadlines.item)?,
        })
    }
}

pub(crate) fn maximum_stage_reserved_bytes(
    fixture: &FrozenFixture,
    window: usize,
) -> FixtureResult<u64> {
    let window_u64 =
        u64::try_from(window).map_err(|_| FixtureError::Invariant("bounded-window"))?;
    let parse_window_bytes = fixture
        .maximum_item_bytes
        .checked_mul(window_u64)
        .ok_or(FixtureError::Invariant("scope-byte-cap"))?;
    Ok(parse_window_bytes.max(fixture.config.validation_bytes[ValidationBudget::Working as usize]))
}

pub(crate) const fn committed_row_counts() -> RowCounts {
    RowCounts {
        files: EXPECTED_FILES,
        symbols: EXPECTED_SYMBOLS,
        edges: EXPECTED_EDGES,
        references: EXPECTED_REFERENCES,
        numerical_sites: EXPECTED_NUMERICAL_SITES,
        documents: EXPECTED_DOCUMENTS,
    }
}

impl BenchmarkConfig {
    pub(crate) fn supervisor_config(&self, max_tasks: usize, max_bytes: u64) -> SupervisorConfig {
        SupervisorConfig::new(self.deadlines.operation)
            .with_heartbeat_interval(self.deadlines.heartbeat_interval)
            .with_heartbeat_timeout(self.deadlines.heartbeat_timeout)
            .with_progress_timeout(self.deadlines.progress)
            .with_cancellation_grace(self.deadlines.cleanup_grace)
            .with_copy_timeout(self.deadlines.copy)
            .with_max_worker_tasks(max_tasks)
            .with_max_worker_bytes(max_bytes)
    }
}

pub(crate) fn fixture_fingerprint(
    config: &BenchmarkConfig,
    inputs: &[FixtureInput],
) -> FixtureResult<String> {
    let mut hasher = blake3::Hasher::new();
    fingerprint_field(&mut hasher, b"cartograph-v2-index-scaling-fixture-v2")?;
    for text in [
        config.fixture_name,
        config.source_revision,
        config.bm25_query,
    ] {
        fingerprint_field(&mut hasher, text.as_bytes())?;
    }
    for number in config.workload {
        fingerprint_usize(&mut hasher, number)?;
    }
    for duration in config.deadlines.fingerprint_values() {
        let nanos = u64::try_from(duration.as_nanos())
            .map_err(|_| FixtureError::Invariant("fixture-duration-fingerprint"))?;
        fingerprint_field(&mut hasher, &nanos.to_le_bytes())?;
    }
    for bytes in config.validation_bytes {
        fingerprint_field(&mut hasher, &bytes.to_le_bytes())?;
    }
    for workers in config.workers {
        fingerprint_field(&mut hasher, &workers.to_le_bytes())?;
    }
    for input in inputs {
        fingerprint_usize(&mut hasher, input.index)?;
        fingerprint_field(&mut hasher, input.source.as_bytes())?;
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn fingerprint_usize(hasher: &mut blake3::Hasher, value: usize) -> FixtureResult<()> {
    let value =
        u64::try_from(value).map_err(|_| FixtureError::Invariant("fixture-number-fingerprint"))?;
    fingerprint_field(hasher, &value.to_le_bytes())
}

fn fingerprint_field(hasher: &mut blake3::Hasher, value: &[u8]) -> FixtureResult<()> {
    let length =
        u64::try_from(value.len()).map_err(|_| FixtureError::Invariant("fixture-field-length"))?;
    hasher.update(&length.to_le_bytes());
    hasher.update(value);
    Ok(())
}

fn fixture_source(index: usize, source_repetitions: usize) -> String {
    let name = qualified_name(index);
    let line = format!(
        "export function {name}(input: number): number {{ const snake_case_value = input + {index:04}; return snake_case_value; }}\n"
    );
    line.repeat(source_repetitions)
}

pub(crate) fn qualified_name(index: usize) -> String {
    if index == 0 {
        "needleCartographBenchmark".to_owned()
    } else {
        format!("fixtureSymbol{index:04}")
    }
}

fn normalized_path(index: usize) -> String {
    format!("src/fixture_{index:04}.ts")
}
