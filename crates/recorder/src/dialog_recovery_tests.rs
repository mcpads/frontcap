use super::*;

#[tokio::test]
async fn dialog_service_does_not_strand_a_lock_owned_by_reconnect() {
    let serial = tokio::sync::Mutex::new(());
    let queried = tokio::sync::Notify::new();
    let reconnect = async {
        let _guard = serial.lock().await;
        queried.notified().await;
        Ok::<_, frontcap_chromium::ChromiumError>(())
    };
    tokio::pin!(reconnect);
    let serviced = std::cell::Cell::new(false);
    let service = async {
        queried.notify_one();
        let _guard = serial.lock().await;
        serviced.set(true);
        Ok(())
    };
    let result = tokio::time::timeout(Duration::from_secs(1), service_with_reconnect(reconnect.as_mut(), service))
        .await.expect("dialog query stranded the reconnect lock owner").unwrap();
    assert_eq!(result, Some(()));
    assert!(serviced.get(), "generation published before admitted dialog service finished");
}
