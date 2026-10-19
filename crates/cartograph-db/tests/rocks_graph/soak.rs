//! Paired topology access under sustained canonical replacement, with durable WAL enabled.

use std::{
    collections::VecDeque,
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use cartograph_db::rocks::{Collection, GraphRetention};

use super::*;

const DURATION: Duration = Duration::from_secs(120);
const MAXIMUM_SAMPLES: usize = 20_000;

#[derive(Default)]
struct ReaderSamples {
    packed_ms: VecDeque<f64>,
    edge_records_ms: VecDeque<f64>,
    verified_snapshots: u64,
}

struct StopReaders<'a>(&'a AtomicBool);

impl Drop for StopReaders<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[test]
#[ignore = "two-minute native storage soak; run explicitly for physical layout changes"]
fn sustained_updates_keep_complete_snapshots_and_compare_packed_with_edge_records() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    store.publish_graph(input(1, soak_facts(1)?)?, &request()?)?;
    let initial_bytes = directory_bytes(&root.path().join(".cartograph/data"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    let (writes, samples) = std::thread::scope(|scope| -> TestResult<_> {
        let readers = (0..3)
            .map(|_| {
                let store = &store;
                let stop = Arc::clone(&stop);
                scope.spawn(move || observe(store, &stop))
            })
            .collect::<Vec<_>>();
        let stop_on_exit = StopReaders(&stop);
        let writes = update_loop(&store, started);
        drop(stop_on_exit);
        let mut samples = Vec::new();
        for reader in readers {
            samples.push(reader.join().map_err(|_| "graph reader panicked")??);
        }
        Ok((writes?, samples))
    })?;
    let mut packed = Vec::new();
    let mut records = Vec::new();
    let mut verified = 0;
    for reader in samples {
        packed.extend(reader.packed_ms);
        records.extend(reader.edge_records_ms);
        verified += reader.verified_snapshots;
    }
    assert!(writes.len() >= 25);
    assert!(verified >= 100);
    let final_bytes = directory_bytes(&root.path().join(".cartograph/data"))?;
    println!(
        "ROCKS_GRAPH_SOAK={}",
        serde_json::json!({
            "durationSeconds": started.elapsed().as_secs_f64(),
            "canonicalPublications": writes.len(),
            "verifiedConcurrentSnapshots": verified,
            "readerThreads": 3,
            "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            "canonicalReadyToCorrectQueryMs": percentiles(writes),
            "packedTopologyQueryMs": percentiles(packed),
            "edgeRecordTopologyQueryMs": percentiles(records),
            "topologyLimit": 64,
            "initialDatabaseBytes": initial_bytes,
            "finalDatabaseBytes": final_bytes,
            "scope": "native storage and synthetic canonical facts; excludes watcher, extraction, lexical and vector retrieval"
        })
    );
    Ok(())
}

fn soak_facts(sequence: u64) -> TestResult<CanonicalGenerationFacts> {
    let mut edges = (2..=2_000)
        .map(|target| edge(1, target, "native"))
        .collect::<Vec<_>>();
    edges.push(edge(
        3_000,
        if sequence.is_multiple_of(2) { 2 } else { 3 },
        "native",
    ));
    facts(3_000, edges)
}

fn update_loop(store: &ProjectStore, started: Instant) -> TestResult<Vec<f64>> {
    let mut samples = Vec::new();
    for sequence in 2..=10_000 {
        let canonical = soak_facts(sequence)?;
        let ready = Instant::now();
        let publication = store.publish_graph(input(sequence, canonical)?, &request()?)?;
        assert!(publication.report.written_records <= 8);
        assert!(publication.report.deleted_records <= 4);
        assert!(matches!(
            publication.retention,
            GraphRetention::Completed(_)
        ));
        let snapshot = store.current_snapshot(Some(&generation(sequence)))?;
        verify_dynamic_edge(&snapshot)?;
        samples.push(ready.elapsed().as_secs_f64() * 1_000.0);
        if started.elapsed() >= DURATION {
            break;
        }
    }
    Ok(samples)
}

fn observe(store: &ProjectStore, stop: &AtomicBool) -> Result<ReaderSamples, RocksError> {
    let mut samples = ReaderSamples::default();
    while !stop.load(Ordering::Acquire) {
        let snapshot = store.current_snapshot(None)?;
        verify_dynamic_edge(&snapshot)?;
        let graph = snapshot.graph(&mut read_budget()?)?;
        let packed = || -> Result<_, RocksError> {
            let start = Instant::now();
            let links = graph
                .neighbors(
                    &symbol(1),
                    GraphDirection::Outgoing,
                    None,
                    64,
                    &mut read_budget()?,
                )?
                .links;
            let ids = links
                .iter()
                .map(|link| uuid_bytes(link.target_symbol_id()))
                .collect::<Result<Vec<_>, _>>()?;
            Ok((start.elapsed().as_secs_f64() * 1_000.0, ids))
        };
        let records = || -> Result<_, RocksError> {
            let mut prefix = b"cg1/e/".to_vec();
            prefix.extend_from_slice(&uuid_bytes(&symbol(1))?);
            let start = Instant::now();
            let page =
                snapshot.scan_prefix(Collection::Graph, &prefix, None, 64, &mut read_budget()?)?;
            let ids = page
                .rows
                .iter()
                .map(|(key, _)| {
                    key.as_bytes()[prefix.len()..prefix.len() + 16]
                        .try_into()
                        .map_err(|_| RocksError::Corrupt {
                            field: "probe_edge_key",
                        })
                })
                .collect::<Result<Vec<[u8; 16]>, _>>()?;
            Ok((start.elapsed().as_secs_f64() * 1_000.0, ids))
        };
        let ((packed_ms, packed_ids), (records_ms, records_ids)) =
            if samples.verified_snapshots.is_multiple_of(2) {
                (packed()?, records()?)
            } else {
                let records = records()?;
                (packed()?, records)
            };
        if packed_ids != records_ids {
            return Err(RocksError::Corrupt {
                field: "probe_paired_topology",
            });
        }
        retain(&mut samples.packed_ms, packed_ms);
        retain(&mut samples.edge_records_ms, records_ms);
        samples.verified_snapshots += 1;
    }
    Ok(samples)
}

fn verify_dynamic_edge(
    snapshot: &cartograph_db::rocks::RevisionSnapshot<'_>,
) -> Result<(), RocksError> {
    let mut budget = read_budget()?;
    let graph = snapshot.graph(&mut budget)?;
    let target = if snapshot.revision().sequence.is_multiple_of(2) {
        2
    } else {
        3
    };
    let links = graph
        .neighbors(
            &symbol(3_000),
            GraphDirection::Outgoing,
            None,
            10,
            &mut budget,
        )?
        .links;
    if links.len() != 1 || links[0].target_symbol_id() != &symbol(target) {
        return Err(RocksError::Corrupt {
            field: "probe_forward_snapshot",
        });
    }
    let reverse = graph
        .neighbors(
            &symbol(target),
            GraphDirection::Incoming,
            None,
            10,
            &mut budget,
        )?
        .links;
    if !reverse
        .iter()
        .any(|link| link.source_symbol_id() == &symbol(3_000))
    {
        return Err(RocksError::Corrupt {
            field: "probe_reverse_snapshot",
        });
    }
    let old = graph
        .neighbors(
            &symbol(if target == 2 { 3 } else { 2 }),
            GraphDirection::Incoming,
            None,
            10,
            &mut budget,
        )?
        .links;
    if old
        .iter()
        .any(|link| link.source_symbol_id() == &symbol(3_000))
    {
        return Err(RocksError::Corrupt {
            field: "probe_removed_snapshot",
        });
    }
    Ok(())
}

fn read_budget() -> Result<ReadBudget, RocksError> {
    ReadBudget::new(10_000, 8 * 1_048_576, Duration::from_secs(30))
}

fn uuid_bytes(id: &SymbolId) -> Result<[u8; 16], RocksError> {
    let raw = id.as_str().replace('-', "");
    let mut bytes = [0; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&raw[index * 2..index * 2 + 2], 16).map_err(|_| {
            RocksError::Corrupt {
                field: "probe_uuid",
            }
        })?;
    }
    Ok(bytes)
}

fn retain(samples: &mut VecDeque<f64>, value: f64) {
    if samples.len() == MAXIMUM_SAMPLES {
        samples.pop_front();
    }
    samples.push_back(value);
}

fn percentiles(mut values: Vec<f64>) -> serde_json::Value {
    values.sort_by(f64::total_cmp);
    serde_json::json!({ "samples": values.len(), "p50": values[values.len() / 2], "p95": values[values.len() * 95 / 100], "p99": values[values.len() * 99 / 100] })
}

fn directory_bytes(path: &Path) -> std::io::Result<u64> {
    let mut total = 0_u64;
    for entry in fs::read_dir(path)? {
        total += entry?.metadata()?.len();
    }
    Ok(total)
}
