//! The unix-socket server: one JSON request line per connection, one JSON
//! response line back.

use std::sync::Arc;

use aqua_messenger::proto::Response;
use aqua_seat::proto::{encode_line, Request};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use crate::daemon::Daemon;

/// Largest request line accepted.
const MAX_REQUEST_BYTES: u64 = 256 * 1024;

pub async fn serve(listener: UnixListener, d: Arc<Daemon>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let d = d.clone();
                tokio::spawn(async move {
                    serve_conn(stream, move |req| async move { d.handle(req).await }).await;
                });
            }
            Err(e) => {
                tracing::warn!("accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

/// One connection: read the request line, run `handle`, write the answer. If the client goes away
/// while `handle` runs (a killed hook leaves its long-poll `Listen` behind), `handle` is dropped
/// at once: a dead client's request must not stay pending and take a wake meant for the next one.
pub async fn serve_conn<F, Fut>(stream: tokio::net::UnixStream, handle: F)
where
    F: FnOnce(Request) -> Fut,
    Fut: std::future::Future<Output = Response>,
{
    let (rd, mut wr) = stream.into_split();
    let mut reader = BufReader::new(rd.take(MAX_REQUEST_BYTES));
    let mut line = String::new();
    let read = reader.read_line(&mut line).await;
    let resp = match read {
        Ok(0) => return,
        Ok(_) => match serde_json::from_str::<Request>(&line) {
            Ok(req) => {
                let mut rd = reader.into_inner().into_inner();
                tokio::select! {
                    r = handle(req) => r,
                    _ = client_gone(&mut rd) => return,
                }
            }
            Err(e) => Response::err(format!("bad request: {e}")),
        },
        Err(e) => Response::err(format!("read: {e}")),
    };
    let _ = wr.write_all(encode_line(&resp).as_bytes()).await;
    let _ = wr.shutdown().await;
}

/// Resolves when the peer closed its end (or the socket failed). Stray bytes are ignored.
async fn client_gone(rd: &mut tokio::net::unix::OwnedReadHalf) {
    let mut buf = [0u8; 64];
    loop {
        match rd.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    struct SetOnDrop(Arc<AtomicBool>);
    impl Drop for SetOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn pair() -> (tokio::net::UnixStream, tokio::net::UnixStream) {
        tokio::net::UnixStream::pair().unwrap()
    }

    #[tokio::test]
    async fn a_request_whose_client_hung_up_is_dropped_at_once() {
        let (server, mut client) = pair();
        let dropped = Arc::new(AtomicBool::new(false));
        let d2 = dropped.clone();
        let task = tokio::spawn(serve_conn(server, move |_req| async move {
            let _guard = SetOnDrop(d2);
            tokio::time::sleep(Duration::from_secs(3600)).await; // a long-poll Listen
            Response::ok(serde_json::json!({}))
        }));
        client
            .write_all(encode_line(&Request::Unread { seat: "seat-x".into() }).as_bytes())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!dropped.load(Ordering::SeqCst), "still waiting while the client is there");
        drop(client);
        tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
        assert!(dropped.load(Ordering::SeqCst), "the handler was dropped when the client left");
    }

    #[tokio::test]
    async fn a_client_that_stays_gets_its_answer() {
        let (server, mut client) = pair();
        let task = tokio::spawn(serve_conn(server, |_req| async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Response::ok(serde_json::json!({"n": 7}))
        }));
        client
            .write_all(encode_line(&Request::Unread { seat: "seat-x".into() }).as_bytes())
            .await
            .unwrap();
        let mut out = String::new();
        BufReader::new(&mut client).read_line(&mut out).await.unwrap();
        assert!(out.contains("\"n\":7"), "{out}");
        task.await.unwrap();
    }
}
