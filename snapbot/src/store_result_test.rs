//! Reproduce the lost durable-PUT response without repeating browser execution.

use super::put_browse_result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

// Read the complete request before dropping its response, matching the observed
// transport ambiguity where the destination may already hold the result.
async fn read_put(stream: TcpStream) -> (TcpStream, String, Vec<u8>) {
    let mut reader = BufReader::new(stream);
    let mut request = String::new();
    reader.read_line(&mut request).await.unwrap();
    assert!(request.starts_with("PUT /private-result?capability=test HTTP/1.1"));
    let mut length = None;
    loop {
        let mut line = String::new();
        assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
    }
    let mut body = vec![0; length.expect("result PUT must have a complete byte body")];
    reader.read_exact(&mut body).await.unwrap();
    (reader.into_inner(), request, body)
}

#[tokio::test]
async fn lost_result_put_response_repeats_only_identical_outcome() {
    // Success and original browser failure both retain their exact outcome when
    // the completed first upload loses its HTTP response.
    for (payload, failed) in [
        (json!({"steps": [{"id": "save-search", "ok": true}]}), false),
        (
            json!({"errorMessage": "navigation timed out", "errorType": "Error", "stackTrace": []}),
            true,
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/private-result?capability=test",
            listener.local_addr().unwrap()
        );
        let receiver = tokio::spawn(async move {
            let (first, _) = listener.accept().await.unwrap();
            let (first, first_request, first_body) = read_put(first).await;
            drop(first); // The write completed, but its acknowledgement never arrives.
            let (second, _) = listener.accept().await.unwrap();
            let (mut second, second_request, second_body) = read_put(second).await;
            assert_eq!(
                first_request, second_request,
                "delivery destination changed"
            );
            assert_eq!(
                first_body, second_body,
                "original outcome was recomputed or changed"
            );
            second
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            serde_json::from_slice::<Value>(&second_body).unwrap()
        });
        // This same absolute budget is supplied once; recovery cannot renew it.
        let deadline = crate::js::now_ms() + 5_000;
        put_browse_result(&url, &payload, failed, Some(deadline))
            .await
            .expect("identical result delivery should recover the dropped response");
        let saved = receiver.await.unwrap();
        assert_eq!(saved["payload"], payload);
        assert_eq!(
            saved["function_error"],
            if failed { "Unhandled" } else { "" }
        );
        assert!(crate::js::now_ms() < deadline);
    }
}
