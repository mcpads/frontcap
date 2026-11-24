use super::*;
use frontcap_recorder_protocol::{Answer, ClientFrame, ServerFrame, frame::{read_frame, write_frame}};
#[tokio::test]
async fn wrong_socket_generation_is_rejected_before_the_target_call() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspace { root: temp.path().into() };
    let id = LaunchId::new("g-expected").unwrap();
    let dir = ws.runtime_dir(&id);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = runtime::socket_path(&dir);
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    runtime::save_runtime(&dir, &runtime::RuntimeRecord { launch_id: id.clone(), state: runtime::RuntimeState::Ready, recorder: frontcap_core::process::ProcessIdentity::current(), browser: None, endpoint: None, socket }).unwrap();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut read = tokio::io::BufReader::new(read);
        let hello: ClientFrame = read_frame(&mut read).await.unwrap().unwrap();
        assert_eq!(hello.call, Call::Hello);
        write_frame(&mut write, &ServerFrame { id: hello.id, answer: Answer::Ok(Reply::Hello { launch_id: LaunchId::new("g-foreign").unwrap() }) }).await.unwrap();
        let next: Option<ClientFrame> = tokio::time::timeout(std::time::Duration::from_secs(2), read_frame(&mut read)).await.unwrap().unwrap();
        assert!(next.is_none(), "no requested operation may reach the foreign recorder");
    });
    let error = Connections::default().call(&ws, &id, Call::Status).await.unwrap_err();
    assert_eq!(error.code, "identity_mismatch");
    peer.await.unwrap();
}
