//! Real discovery, `RocksDB` publication and BM25 at every supported worker count.
#[path = "../test_support/dependency_ownership.rs"]
mod dependency_ownership;

use std::{error::Error, fs, time::Duration};

use cartograph_db::{
    CurrentGenerationLookup, NativeDatabase, SearchQuery,
    rocks::{
        AsyncProjectStore, GraphBase, GraphPolicy, GraphRevision, StorageRequest, StorageRuntime,
        StorePolicy, WorkerPolicy,
    },
};
use cartograph_domain::{ContentDigest, GenerationDigestVersion, GenerationId};
use cartograph_extract::{DiscoveryLimits, SourceLimits, SourceRoot};
use cartograph_indexer::{
    NativeGenerationBuild, NativePipelineConfig, NativePipelineDeadlines, NativePipelineLimits,
    NativePipelineParallelism, NativeRetainedLimits, StageCapacity, build_native_rocks_graph_owned,
};

const FILES: [(&str, &str); 5] = [
    (
        "freeswitch/dialplan/default.xml",
        include_str!(
            "../../cartograph-extract/tests/fixtures/telephony/freeswitch/dialplan/default.xml"
        ),
    ),
    (
        "freeswitch/sip_profiles/internal.xml",
        include_str!(
            "../../cartograph-extract/tests/fixtures/telephony/freeswitch/sip_profiles/internal.xml"
        ),
    ),
    (
        "kamailio/kamailio.cfg",
        include_str!("../../cartograph-extract/tests/fixtures/telephony/kamailio/kamailio.cfg"),
    ),
    (
        "kamailio/auth.cfg",
        include_str!("../../cartograph-extract/tests/fixtures/telephony/kamailio/auth.cfg"),
    ),
    (
        "rtpengine/rtpengine.conf",
        include_str!("../../cartograph-extract/tests/fixtures/telephony/rtpengine/rtpengine.conf"),
    ),
];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn telephony_publishes_searchable_identical_facts_at_all_worker_counts()
-> Result<(), Box<dyn Error>> {
    let source = tempfile::tempdir()?;
    for (path, contents) in FILES {
        let path = source.path().join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)?;
    }
    let mut baseline = None;
    for workers in [1_u16, 2, 4, 8, 16] {
        let directory = tempfile::tempdir()?;
        let request = StorageRequest::new(Duration::from_secs(60))?;
        let runtime = StorageRuntime::new(WorkerPolicy::default())?;
        let identity = ContentDigest::from_bytes([17; 32]);
        let store = AsyncProjectStore::open(
            &runtime,
            directory.path().to_owned(),
            identity.clone(),
            StorePolicy::default(),
            &request,
        )
        .await?;
        let database = NativeDatabase::new(store.clone(), &identity);
        let (input, report) = build_native_rocks_graph_owned(
            NativeGenerationBuild::new(SourceRoot::open(source.path())?, pipeline_config(workers)?),
            GraphRevision {
                generation_id: GenerationId::from_uuid_v8([17; 16]),
                source_revision: ContentDigest::from_bytes([31; 32]),
                base: GraphBase::Empty,
            },
            GraphPolicy::default(),
            &request,
        )
        .await?;
        assert_eq!(report.discovered_files(), 5);
        let published = store.publish_graph(input, &request).await?;
        assert_eq!(
            published.revision.digest_version,
            Some(GenerationDigestVersion::V25)
        );
        let mut identities = Vec::new();
        for (query, qualified) in [
            (
                "inbound",
                "freeswitch::context::default::extension::inbound",
            ),
            ("carrier", "freeswitch::profile::internal::gateway::carrier"),
            ("AUTH", "kamailio::route::AUTH"),
            ("listen-ng", "rtpengine::rtpengine::listen-ng"),
            ("WebRTC", "rtpengine::templates::WebRTC"),
        ] {
            let hits = database
                .search_current_code(SearchQuery::new(
                    CurrentGenerationLookup::new(
                        database.project_id(),
                        &published.revision.generation_id,
                    ),
                    query,
                    20,
                ))
                .await?;
            assert!(
                hits.iter().any(|hit| hit.qualified_name() == qualified),
                "{workers} workers: {qualified}"
            );
            assert!(!format!("{hits:?}").contains("telephony_fixture_value_sentinel_834"));
            identities.push(
                hits.iter()
                    .map(|hit| hit.document_id().to_string())
                    .collect::<Vec<_>>(),
            );
        }
        let observation = (published.revision.content_digest.clone(), identities);
        if let Some(baseline) = &baseline {
            assert_eq!(&observation, baseline, "{workers} workers");
        } else {
            baseline = Some(observation);
        }
        runtime.shutdown(&request).await?;
    }
    Ok(())
}

fn pipeline_config(workers: u16) -> Result<NativePipelineConfig, Box<dyn Error>> {
    let capacity = StageCapacity::new(usize::from(workers), usize::from(workers));
    Ok(NativePipelineConfig::new(
        NativePipelineLimits::new(
            DiscoveryLimits::new(128, 1024 * 1024)?,
            SourceLimits::new(1024 * 1024)?,
            NativeRetainedLimits::new(4 * 1024 * 1024, 32 * 1024 * 1024)?,
        ),
        NativePipelineParallelism::new(capacity, capacity)?,
        NativePipelineDeadlines::new(
            Duration::from_secs(10),
            Duration::from_secs(60),
            Duration::from_millis(500),
        )?,
    ))
}
