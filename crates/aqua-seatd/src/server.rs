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
            Ok((mut stream, _)) => {
                let d = d.clone();
                tokio::spawn(async move {
                    let mut line = String::new();
                    let read = {
                        let mut reader = BufReader::new((&mut stream).take(MAX_REQUEST_BYTES));
                        reader.read_line(&mut line).await
                    };
                    let resp = match read {
                        Ok(0) => return,
                        Ok(_) => match serde_json::from_str::<Request>(&line) {
                            Ok(req) => d.handle(req).await,
                            Err(e) => Response::err(format!("bad request: {e}")),
                        },
                        Err(e) => Response::err(format!("read: {e}")),
                    };
                    let _ = stream.write_all(encode_line(&resp).as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
            Err(e) => {
                tracing::warn!("accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}
