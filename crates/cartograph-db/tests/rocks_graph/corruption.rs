use cartograph_db::rocks::{Collection, RecordChange, RecordKey};
use cartograph_domain::GenerationDigestVersion;

use super::*;

fn replace_raw(store: &ProjectStore, key: RecordKey, bytes: Vec<u8>) -> TestResult {
    let snapshot = store.current_snapshot(None)?;
    let digest = snapshot
        .revision()
        .content_digest
        .clone()
        .ok_or("missing graph digest")?;
    let writer = store.begin_revision(generation(2), super::digest(9), Duration::from_secs(30))?;
    store.stage(&writer, &[RecordChange::put(Collection::Graph, key, bytes)])?;
    store.seal(&writer, digest, GenerationDigestVersion::V23)?;
    store.publish(&writer)?;
    Ok(())
}

#[test]
fn topology_traversal_does_not_load_properties_and_hydration_rejects_corruption() -> TestResult {
    for invalid in [
        b"not json".as_slice(),
        br#"{"confidence":2.0,"provenance":"native","site_count":3}"#,
        br#"{"confidence":0.8,"provenance":"native","site_count":0}"#,
        br#"{"confidence":0.8,"provenance":"different","site_count":3}"#,
    ] {
        let root = tempfile::tempdir()?;
        let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
        store.publish_graph(
            input(1, facts(2, vec![edge(1, 2, "native")])?)?,
            &request()?,
        )?;
        let old = store.current_snapshot(None)?;
        let key = old
            .scan_prefix(Collection::Graph, b"cg1/e/", None, 1, &mut budget()?)?
            .rows
            .remove(0)
            .0;
        replace_raw(&store, key, invalid.to_vec())?;
        let snapshot = store.current_snapshot(None)?;
        let graph = snapshot.graph(&mut budget()?)?;
        let page = graph.neighbors(
            &symbol(1),
            GraphDirection::Outgoing,
            None,
            1,
            &mut budget()?,
        )?;
        assert_eq!(page.links.len(), 1);
        assert_eq!(page.links[0].target_symbol_id(), &symbol(2));
        assert!(matches!(
            graph.edge(&page.links[0], &mut budget()?),
            Err(RocksError::Corrupt { .. })
        ));
        let previous = old.graph(&mut budget()?)?;
        let link = previous
            .neighbors(
                &symbol(1),
                GraphDirection::Outgoing,
                None,
                1,
                &mut budget()?,
            )?
            .links
            .remove(0);
        assert_eq!(previous.edge(&link, &mut budget()?)?.provenance, "native");
    }
    Ok(())
}

#[test]
fn malformed_packed_blocks_fail_without_returning_partial_topology() -> TestResult {
    for defect in 0..7 {
        let root = tempfile::tempdir()?;
        let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
        store.publish_graph(
            input(1, facts(2, vec![edge(1, 2, "native")])?)?,
            &request()?,
        )?;
        let snapshot = store.current_snapshot(None)?;
        let (key, mut bytes) = snapshot
            .scan_prefix(Collection::Graph, b"cg1/o/", None, 1, &mut budget()?)?
            .rows
            .remove(0);
        match defect {
            0 => bytes[0] = b'X',
            1 => {
                bytes.pop();
            }
            2 => bytes[4..6].copy_from_slice(&393_u16.to_be_bytes()),
            3 => bytes[4..6].copy_from_slice(&1_u16.to_be_bytes()),
            4 => bytes[22] = 255,
            5 => bytes[6..22].fill(0),
            _ => bytes.extend_from_within(6..),
        }
        replace_raw(&store, key, bytes)?;
        let current = store.current_snapshot(None)?;
        let graph = current.graph(&mut budget()?)?;
        assert!(matches!(
            graph.neighbors(
                &symbol(1),
                GraphDirection::Outgoing,
                None,
                20,
                &mut budget()?
            ),
            Err(RocksError::Corrupt { .. })
        ));
        assert_eq!(
            snapshot
                .graph(&mut budget()?)?
                .neighbors(
                    &symbol(1),
                    GraphDirection::Outgoing,
                    None,
                    20,
                    &mut budget()?
                )?
                .links
                .len(),
            1
        );
    }
    Ok(())
}

#[test]
fn unsupported_graph_manifests_never_allow_an_implicit_replacement() -> TestResult {
    for invalid in [
        b"not json".to_vec(),
        br#"{"format_version":2,"content_digest":"0000000000000000000000000000000000000000000000000000000000000000","files":1,"symbols":2,"edges":1,"references":0,"numerical_sites":0,"documents":0}"#.to_vec(),
        br#"{"format_version":1,"content_digest":"0000000000000000000000000000000000000000000000000000000000000000","files":1,"symbols":2,"edges":1,"references":0,"numerical_sites":0,"documents":0}"#.to_vec(),
    ] {
        let root = tempfile::tempdir()?;
        let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
        store.publish_graph(input(1, facts(2, vec![edge(1, 2, "native")])?)?, &request()?)?;
        replace_raw(&store, RecordKey::new(b"cg1/m".to_vec())?, invalid)?;
        assert!(matches!(
            store.publish_graph(input(3, facts(2, vec![])?)?, &request()?).map_err(|error| error.primary),
            Err(RocksError::Corrupt { .. } | RocksError::Format)
        ));
        assert_eq!(store.current_snapshot(None)?.revision().generation_id, generation(2));
    }
    Ok(())
}
