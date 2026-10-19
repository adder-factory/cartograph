use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edited_file_reparses_while_unchanged_file_reuses_native_extraction()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    std::fs::write(root.path().join("first.rs"), "pub fn first() {}\n")?;
    std::fs::write(root.path().join("second.rs"), "pub fn second() {}\n")?;
    let runtime =
        RocksProjectRuntime::open(root.path(), &StorageRequest::new(Duration::from_secs(30))?)
            .await?;
    let first = runtime
        .index(
            RocksIndexOptions::default(),
            &StorageRequest::new(Duration::from_secs(30))?,
        )
        .await?;
    let cache = first.parse_cache.ok_or("missing first extraction report")?;
    assert_eq!(cache.hits(), 0);
    assert_eq!(cache.parsed_files(), 2);
    assert_eq!(cache.writes(), 2);
    assert_eq!(cache.write_errors(), 0);
    std::fs::write(root.path().join("first.rs"), "pub fn replacement() {}\n")?;
    let second = runtime
        .index(
            RocksIndexOptions::default(),
            &StorageRequest::new(Duration::from_secs(30))?,
        )
        .await?;
    assert!(second.fresh);
    assert_ne!(first.revision.generation_id, second.revision.generation_id);
    let cache = second
        .parse_cache
        .ok_or("missing incremental extraction report")?;
    assert_eq!(cache.hits(), 1);
    assert_eq!(cache.misses(), 1);
    assert_eq!(cache.parsed_files(), 1);
    assert_eq!(cache.read_errors(), 0);
    assert_eq!(cache.write_errors(), 0);
    runtime.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn increasing_configured_file_limit_invalidates_the_graph_and_explicit_override_survives_reopen()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    std::fs::create_dir(root.path().join(".cartograph"))?;
    let config = root.path().join(".cartograph/config.json");
    std::fs::write(&config, r#"{"maxFileSize":1024}"#)?;
    let source = format!(
        "pub fn admitted_after_raise() {{}}\n//{}\n",
        "x".repeat(1500)
    );
    std::fs::write(root.path().join("large.rs"), source)?;
    let request = StorageRequest::new(Duration::from_secs(60))?;
    let runtime = RocksProjectRuntime::open(root.path(), &request).await?;
    let first = runtime
        .index(RocksIndexOptions::default(), &request)
        .await?;
    assert!(first.fresh);
    assert_eq!(first.revision.maximum_file_bytes, None);
    assert_eq!(first.counts.symbols, 0);
    std::fs::write(&config, r#"{"maxFileSize":2048}"#)?;
    assert!(
        !runtime.status(&request).await?.fresh,
        "newly eligible source must make the old graph stale"
    );
    let second = runtime
        .index(RocksIndexOptions::default(), &request)
        .await?;
    assert!(!second.unchanged);
    assert!(second.fresh);
    let matches = runtime
        .database()
        .exact_current_symbols_by_name(cartograph_db::ExactTextLookup::new(
            cartograph_db::CurrentGenerationLookup::new(
                runtime.database().project_id(),
                &second.revision.generation_id,
            ),
            "admitted_after_raise",
            10,
        ))
        .await?;
    assert_eq!(matches.len(), 1);
    assert_eq!(second.revision.maximum_file_bytes, None);
    runtime.shutdown().await?;
    drop(runtime);
    let runtime = RocksProjectRuntime::open(root.path(), &request).await?;
    assert!(runtime.status(&request).await?.fresh);
    let explicit = runtime
        .index(
            RocksIndexOptions {
                maximum_file_bytes: Some(1024),
                ..RocksIndexOptions::default()
            },
            &request,
        )
        .await?;
    assert_eq!(explicit.revision.maximum_file_bytes, Some(1024));
    assert_eq!(explicit.counts.symbols, 0);
    runtime.shutdown().await?;
    drop(runtime);
    std::fs::write(&config, r#"{"maxFileSize":4096}"#)?;
    let runtime = RocksProjectRuntime::open(root.path(), &request).await?;
    assert!(
        runtime.status(&request).await?.fresh,
        "explicit run admission must survive reopen"
    );
    let inherited = runtime
        .index(RocksIndexOptions::default(), &request)
        .await?;
    assert!(inherited.unchanged);
    assert_eq!(inherited.revision.maximum_file_bytes, Some(1024));
    runtime.shutdown().await?;
    Ok(())
}
