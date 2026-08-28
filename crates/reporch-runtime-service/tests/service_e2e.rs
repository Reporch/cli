#![cfg(unix)]

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use reporch_runtime_protocol::{
    PROTOCOL_VERSION, RuntimeServiceCommandV1, RuntimeServiceRequestV1, RuntimeServiceResultV1,
    SERVICE_REQUEST_SCHEMA, read_service_response, write_service_request,
};
use tokio::net::UnixStream;
use tokio::process::Command;
use uuid::Uuid;

async fn connect_when_ready(path: &Path) -> UnixStream {
    for _ in 0..100 {
        if let Ok(stream) = UnixStream::connect(path).await {
            return stream;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("runtime service did not create its socket");
}

#[tokio::test]
async fn current_user_can_ping_the_private_runtime_service() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("service.sock");
    let spool = root.path().join("spool");
    let mut child = Command::new(env!("CARGO_BIN_EXE_reporch-runtime-service"))
        .env("REPORCH_RUNTIME_SERVICE_SOCKET", &socket)
        .env("REPORCH_RUNTIME_SPOOL_ROOT", &spool)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stream = connect_when_ready(&socket).await;
    let request = RuntimeServiceRequestV1 {
        schema: SERVICE_REQUEST_SCHEMA.into(),
        protocol_version: PROTOCOL_VERSION,
        id: Uuid::now_v7(),
        command: RuntimeServiceCommandV1::Ping,
    };
    write_service_request(&mut stream, &request).await.unwrap();
    let response = read_service_response(&mut stream).await.unwrap();
    response.validate_for(&request).unwrap();
    assert!(matches!(
        response.result,
        RuntimeServiceResultV1::Pong { .. }
    ));
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}
