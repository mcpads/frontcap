use super::*;
use std::sync::Arc;
use frontcap_core::write_budget::{Limits, WriteBudget};

#[tokio::test]
async fn cancellation_requires_matching_owner_and_durable_retirement_admission() {
    for foreign in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::bind(root.path()).await.unwrap();
        let launch = LaunchId::generate();
        let request = Cancel { launch_id: if foreign { LaunchId::generate() } else { launch.clone() },
            request_id: RequestId::new("cancel").unwrap() };
        let budget = WriteBudget::create(&root.path().join("budget"), launch.clone(),
            Limits { ordinary: 1, retirement: 1 }).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let (finish, waiting) = tokio::sync::oneshot::channel();
        let owner = tokio::spawn(async move {
            let task = tokio::spawn(async { waiting.await.map_err(io::Error::other) });
            endpoint.acquire(task, None, &launch, &flag, Some(&budget)).await
        });
        let stream = UnixStream::connect(socket(root.path())).await.unwrap();
        let mut stream = BufReader::new(stream);
        write_frame(stream.get_mut(), &request).await.unwrap();
        let reply: Option<Cancellation> = tokio::time::timeout(Duration::from_secs(2),
            read_frame(&mut stream)).await.unwrap().unwrap();
        if foreign { assert!(matches!(reply, Some(Cancellation::NotSent {..}))); }
        else { assert!(reply.is_none()); } // Receipt refusal never dispatches cancellation.
        assert!(!cancelled.load(Ordering::Acquire));
        assert!(!cancellation_path(root.path()).exists());
        finish.send(()).unwrap();
        owner.await.unwrap().unwrap();
    }
}
