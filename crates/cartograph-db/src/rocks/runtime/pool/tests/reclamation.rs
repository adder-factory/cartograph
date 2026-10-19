use super::*;

#[tokio::test(flavor = "current_thread")]
async fn reclaimed_bytes_preserve_floor_reply_and_fifo_reader_admission() -> TestResult {
    let pool = pool()?;
    let request = request()?;
    assert!(
        pool.run_reclaiming(&request, 10, true, 11, |_, _| Ok(()))
            .await
            .is_err()
    );
    assert_eq!(pool.bytes.available_permits(), 1_048_576);
    let mut payload = Vec::new();
    payload.try_reserve_exact(500_000)?;
    payload.resize(500_000, 42_u8);
    let capacity = payload.capacity();
    let credit_limit = u32::try_from(capacity)?;
    assert!((500_000..=600_000).contains(&credit_limit));
    let reservation = 400_000 + credit_limit;
    let (entered, started) = oneshot::channel();
    let (destroy, destroy_now) = mpsc::channel();
    let (credited, credit_done) = oneshot::channel();
    let (finish, finish_now) = mpsc::channel();
    let (finished, finished_now) = oneshot::channel();
    let mut writer = Box::pin(pool.run_reclaiming(
        &request,
        reservation,
        true,
        credit_limit,
        move |_, credit| {
            assert!(credit.release(capacity + 1).is_err());
            let _entered = entered.send(());
            destroy_now
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| operation("fixture-gate"))?;
            drop(payload);
            credit.release(capacity)?;
            assert!(credit.release(1).is_err());
            let _credited = credited.send(());
            finish_now
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| operation("fixture-gate"))?;
            let _finished = finished.send(());
            Ok(42)
        },
    ));
    assert!(futures_util::poll!(writer.as_mut()).is_pending());
    tokio::time::timeout(Duration::from_secs(2), started).await??;
    assert_eq!(
        pool.bytes.available_permits(),
        1_048_576 - usize::try_from(reservation)?
    );
    let mut large = Box::pin(pool.run(&request, 300_000, false, |_| Ok(7)));
    assert!(futures_util::poll!(large.as_mut()).is_pending());
    let mut small = Box::pin(pool.run(&request, 64_000, false, |_| Ok(8)));
    // FIFO keeps the small request behind the earlier large waiter.
    assert!(futures_util::poll!(small.as_mut()).is_pending());
    destroy.send(())?;
    tokio::time::timeout(Duration::from_secs(2), credit_done).await??;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), large).await??,
        7
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), small).await??,
        8
    );
    assert_eq!(pool.bytes.available_permits(), 648_576);
    assert!(futures_util::poll!(writer.as_mut()).is_pending());
    finish.send(())?;
    tokio::time::timeout(Duration::from_secs(2), finished_now).await??;
    // The real completion, then its unread Reply, retain the 400000-byte floor.
    assert_eq!(pool.bytes.available_permits(), 648_576);
    assert_eq!(writer.await?, 42);
    assert_eq!(pool.bytes.available_permits(), 1_048_576);
    pool.close();
    pool.drain(&drain_request()?).await?;
    Ok(())
}
