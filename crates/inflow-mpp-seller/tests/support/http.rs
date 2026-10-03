use super::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[tokio::test]
async fn direct_http_handler_preserves_authentication_body_binding_and_platform_errors() {
    tokio::time::timeout(std::time::Duration::from_secs(15), exercise())
        .await
        .unwrap();
}

async fn exercise() {
    let (seller, script) = seller(vec![
        Reply::Config(config()),
        Reply::Validate(json!({})),
        Reply::Receipt(receipt()),
        Reply::Status(403),
    ])
    .await;
    let offer = seller
        .offer(
            Method::Inflow,
            json!({"amount":"1.25","currency":"USDC"}),
            ChallengeOptions {
                requires_auth: true,
                description: Some("Search".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/search", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut deliveries = 0;
        for _ in 0..5 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                bytes.push(stream.read_u8().await.unwrap());
            }
            let headers = String::from_utf8(bytes).unwrap();
            assert!(headers.starts_with("POST /search "));
            let header = |name: &str| {
                headers.lines().find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case(name).then_some(value.trim())
                })
            };
            assert!(header("x-api-key").is_none());
            let length = header("content-length").unwrap().parse::<usize>().unwrap();
            assert!(length <= 1024);
            let mut body = vec![0; length];
            stream.read_exact(&mut body).await.unwrap();
            let (status, extra, response) =
                if header("authorization") != Some("Bearer service-session") {
                    (401, String::new(), "authenticate first".to_owned())
                } else if let Some(payment) = header("payment-authorization") {
                    let credential =
                        inflow_mpp::decode_credential(payment.strip_prefix("Payment ").unwrap())
                            .unwrap();
                    match offer
                        .accept(&credential, Some(&body), &CancellationToken::new())
                        .await
                    {
                        Ok(receipt) => {
                            deliveries += 1;
                            let receipt = encode(&serde_json::to_value(receipt).unwrap()).unwrap();
                            (
                                200,
                                format!("Payment-Receipt: {receipt}\r\n"),
                                "paid result".into(),
                            )
                        }
                        Err(error) if error.code == "MPP_CREDENTIAL_MISMATCH" => {
                            (402, String::new(), error.code)
                        }
                        Err(_) => (
                            503,
                            String::new(),
                            "payment infrastructure unavailable".into(),
                        ),
                    }
                } else {
                    (
                        402,
                        format!(
                            "WWW-Authenticate: {}\r\n",
                            inflow_mpp::render_challenge(&offer.challenge(Some(&body)).unwrap())
                                .unwrap()
                        ),
                        String::new(),
                    )
                };
            stream.write_all(format!("HTTP/1.1 {status} Response\r\n{extra}Cache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).as_bytes()).await.unwrap();
        }
        deliveries
    });
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    assert_eq!(
        http.post(&url).body("query").send().await.unwrap().status(),
        401
    );
    let response = http
        .post(&url)
        .bearer_auth("service-session")
        .body("query")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 402);
    let challenge =
        inflow_mpp::parse_challenges(&[response.headers()["www-authenticate"].to_str().unwrap()])
            .unwrap()
            .remove(0);
    assert_eq!(challenge.header.as_deref(), Some("Payment-Authorization"));
    assert_eq!(challenge.description.as_deref(), Some("Search"));
    let credential = Credential {
        challenge,
        source: None,
        payload: serde_json::Map::from_iter([("proof".into(), json!("synthetic"))]),
    };
    let payment = format!(
        "Payment {}",
        inflow_mpp::encode_credential(&credential).unwrap()
    );
    let send = |body: &'static str| {
        http.post(&url)
            .bearer_auth("service-session")
            .header("Payment-Authorization", &payment)
            .body(body)
            .send()
    };
    assert_eq!(send("changed query").await.unwrap().status(), 402);
    let response = send("query").await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        inflow_mpp::decode_receipt(response.headers()["payment-receipt"].to_str().unwrap())
            .unwrap()
            .reference,
        "tx"
    );
    assert_eq!(response.text().await.unwrap(), "paid result");
    let response = send("query").await.unwrap();
    assert_eq!(response.status(), 503);
    assert!(!response.headers().contains_key("www-authenticate"));
    assert!(!response.headers().contains_key("payment-receipt"));
    assert_eq!(task.await.unwrap(), 1);
    let requests = script.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert!(requests[1].url.ends_with("/v1/mpp/validate"));
    assert!(requests[2].url.ends_with("/v1/mpp/broadcast"));
    assert_eq!(requests[1].body, requests[2].body);
}
