//! Real extraction and both resolution branches retain identical event calls.

use super::*;

const FIXTURES: &[(&str, &str)] = &[
    (
        "ios/Emitter.m",
        "@implementation Emitter\n- (void)publish { [self sendEventWithName:@\"ready\" body:nil]; }\n- (void)unmatched { [self sendEventWithName:@\"missing\" body:nil]; }\n@end",
    ),
    ("src/handlers.ts", "export function onReady() {}"),
    (
        "src/a.ts",
        "import {NativeEventEmitter} from 'react-native'; const emitter = new NativeEventEmitter(); import {onReady} from './handlers'; emitter.on('ready', onReady); emitter.on('ready', onReady);",
    ),
    (
        "src/b.ts",
        "import {NativeEventEmitter} from 'react-native'; const emitter = new NativeEventEmitter(); import {onReady} from './handlers'; emitter.once('ready', onReady); emitter.addListener('ready', onReady); emitter.on('unmatched', onReady);",
    ),
];

fn assert_event_call(facts: &CanonicalGenerationFacts) {
    let source = capability_symbol(facts, "ios/Emitter.m", "Emitter::publish");
    let target = capability_symbol(facts, "src/handlers.ts", "onReady");
    let calls = facts
        .edges()
        .iter()
        .filter(|edge| edge.provenance == native_event_calls::PROVENANCE)
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].source_symbol_id, source.symbol_id);
    assert_eq!(calls[0].target_symbol_id, target.symbol_id);
    assert_eq!(calls[0].kind, EdgeKind::Calls);
    assert_eq!(
        calls[0].confidence,
        native_bridge_details::CONVENTION_CONFIDENCE
    );
    assert_eq!(calls[0].site_count, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_native_resolution_keeps_deduplicated_event_calls_without_sites() {
    let directory = tempdir().unwrap_or_else(|error| panic!("event directory: {error}"));
    for (path, source) in FIXTURES {
        let target = directory.path().join(path);
        fs::create_dir_all(target.parent().unwrap_or(directory.path()))
            .unwrap_or_else(|error| panic!("event parent: {error}"));
        fs::write(target, source).unwrap_or_else(|error| panic!("event fixture: {error}"));
    }
    for retain_sites in [true, false] {
        let policy = config(SERIAL_WORKERS)
            .with_page_rank(false)
            .with_betweenness(false)
            .with_call_sites(retain_sites);
        let serial = build_with_config(directory.path(), SERIAL_WORKERS, policy).await;
        assert_event_call(serial.facts());
        for workers in [2, 4, 8, 16] {
            let policy = config(workers)
                .with_page_rank(false)
                .with_betweenness(false)
                .with_call_sites(retain_sites);
            let parallel = build_with_config(directory.path(), workers, policy).await;
            assert_event_call(parallel.facts());
            assert_eq!(serial.facts().digest(), parallel.facts().digest());
            assert_eq!(serial.facts().edges(), parallel.facts().edges());
            assert_eq!(serial.facts().references(), parallel.facts().references());
            if !retain_sites {
                assert_eq!(parallel.facts().references(), &[]);
            }
        }
    }
}
