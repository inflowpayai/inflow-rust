use super::*;
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

async fn serve(reply: Vec<u8>, delay: Duration) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut byte = [0];
            if stream.read(&mut byte).await.unwrap() == 0 {
                break;
            }
            request.push(byte[0]);
            if request.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        tokio::time::sleep(delay).await;
        let _ = stream.write_all(&reply).await;
        request
    });
    (url, task)
}

fn request(url: String) -> TransportRequest {
    TransportRequest {
        method: Method::GET,
        url,
        headers: HeaderMap::new(),
        body: Vec::new(),
    }
}

#[tokio::test]
async fn real_transport_reads_chunked_bodies_and_does_not_follow_redirects() {
    let client = ReqwestTransport::new().unwrap();
    let (url,task) = serve(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n".to_vec(),Duration::ZERO).await;
    let response = client.send(request(url)).await.unwrap();
    assert_eq!(response.body, b"abcde");
    assert_eq!(response.status, 200);
    task.await.unwrap();
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (url,task) = serve(format!("HTTP/1.1 302 Found\r\nLocation: http://{}/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", target.local_addr().unwrap()).into_bytes(),Duration::ZERO).await;
    let mut input = request(url);
    input.headers.insert(
        "x-api-key",
        http::HeaderValue::from_static("test-only-secret"),
    );
    assert_eq!(client.send(input).await.unwrap().status, 302);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), target.accept())
            .await
            .is_err()
    );
    assert!(
        String::from_utf8(task.await.unwrap())
            .unwrap()
            .contains("test-only-secret")
    );
}

#[tokio::test]
async fn real_transport_reports_disconnect_timeout_and_body_limit() {
    let client = ReqwestTransport::new().unwrap();
    let (url, task) = serve(
        b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\nshort".to_vec(),
        Duration::ZERO,
    )
    .await;
    assert!(matches!(
        client.send(request(url)).await,
        Err(TransportError::Network)
    ));
    task.await.unwrap();
    let (url, task) = serve(Vec::new(), Duration::ZERO).await;
    assert!(matches!(
        client.send(request(url)).await,
        Err(TransportError::Network)
    ));
    task.await.unwrap();
    let timed = ReqwestTransport(
        reqwest::Client::builder()
            .timeout(Duration::from_millis(10))
            .build()
            .unwrap(),
    );
    let (url, task) = serve(Vec::new(), Duration::from_secs(1)).await;
    assert!(matches!(
        timed.send(request(url)).await,
        Err(TransportError::Timeout)
    ));
    task.abort();
    let body = vec![b'x'; MAX_BODY_BYTES + 1];
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend(body);
    let (url, task) = serve(response, Duration::ZERO).await;
    assert!(matches!(
        client.send(request(url)).await,
        Err(TransportError::BodyTooLarge)
    ));
    task.await.unwrap();
}
