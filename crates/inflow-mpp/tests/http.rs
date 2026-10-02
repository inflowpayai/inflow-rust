use inflow_core::{ClientOptions, Transport, TransportError, TransportRequest, TransportResponse};
use inflow_mpp::internal::MppClient;
use serde_json::json;
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Notify,
};
use tokio_util::sync::CancellationToken;

// Only the destination is redirected to loopback. Requests and responses use real HTTP.
struct Loopback {
    base: String,
    http: reqwest::Client,
}
impl Transport for Loopback {
    fn send(
        &self,
        request: TransportRequest,
    ) -> Pin<Box<dyn Future<Output = Result<TransportResponse, TransportError>> + Send + '_>> {
        Box::pin(async move {
            let url = reqwest::Url::parse(&request.url).unwrap();
            let response = self
                .http
                .request(request.method, format!("{}{}", self.base, url.path()))
                .headers(request.headers)
                .body(request.body)
                .send()
                .await
                .map_err(|_| TransportError::Network)?;
            let status = response.status().as_u16();
            let headers = response.headers().clone();
            let body = response
                .bytes()
                .await
                .map_err(|_| TransportError::Network)?
                .to_vec();
            Ok(TransportResponse {
                status,
                headers,
                body,
            })
        })
    }
}

#[tokio::test]
async fn cancel_real_http_body_during_create_poll_and_authorize_then_reuse_client() {
    for phase in 0..3 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let server_started = started.clone();
        let server_release = release.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 1024];
            while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buffer).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
            }
            let path = match phase {
                0 => "/v1/transactions/mpp",
                1 => "/v1/transactions/tx/mpp",
                _ => "/v1/subscriptions/sub/authorize",
            };
            assert!(String::from_utf8_lossy(&bytes).contains(path));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n{")
                .await
                .unwrap();
            server_started.notify_one();
            server_release.notified().await;
            drop(stream);
            let (mut stream, _) = listener.accept().await.unwrap();
            let n = stream.read(&mut buffer).await.unwrap();
            assert!(n > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .await
                .unwrap();
        });
        let client = MppClient::new(ClientOptions {
            transport: Some(Arc::new(Loopback {
                base,
                http: reqwest::Client::new(),
            })),
            ..Default::default()
        })
        .unwrap();
        let cancellation = CancellationToken::new();
        let operation = async {
            match phase {
                0 => client.create(json!({}), &cancellation).await,
                1 => client.transaction("tx", &cancellation).await,
                _ => client.authorize("sub", json!({}), &cancellation).await,
            }
        };
        let cancel = async {
            started.notified().await;
            cancellation.cancel();
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(operation, cancel)
        })
        .await
        .unwrap();
        assert_eq!(result.unwrap_err().code, "CANCELLED");
        release.notify_one();
        assert_eq!(
            client.config(&CancellationToken::new()).await.unwrap(),
            json!({})
        );
        server.await.unwrap();
    }
}
