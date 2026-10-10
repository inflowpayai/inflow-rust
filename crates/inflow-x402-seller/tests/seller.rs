use inflow_core::{Transport, TransportError, TransportRequest, TransportResponse};
use inflow_x402_seller::*;
use serde_json::{Value, json};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct State {
    config: Value,
    supported: Value,
    requests: Vec<TransportRequest>,
    fail: bool,
}
#[derive(Clone)]
struct Server(Arc<Mutex<State>>);
impl Transport for Server {
    fn send(
        &self,
        request: TransportRequest,
    ) -> Pin<Box<dyn Future<Output = Result<TransportResponse, TransportError>> + Send + '_>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            let mut s = self.0.lock().unwrap();
            let body = if request.url.ends_with("/config") {
                s.config.clone()
            } else if request.url.ends_with("/supported") {
                s.supported.clone()
            } else if request.url.ends_with("/verify") {
                json!({"isValid":true,"payer":"buyer","extensions":{"future":1}})
            } else {
                json!({"success":true,"transaction":"tx","network":"inflow:1"})
            };
            s.requests.push(request);
            Ok(TransportResponse {
                status: if s.fail { 403 } else { 200 },
                headers: Default::default(),
                body: serde_json::to_vec(&body).unwrap(),
            })
        })
    }
}
fn fixture() -> Server {
    Server(Arc::new(Mutex::new(State {
        config: json!({"assets":[{"blockchain":"BASE","currency":"USDC","assetName":"USDC","assetId":"token","decimals":6,"network":"eip155:8453","assetTransferMethod":"permit2","permit2Proxy":"0x402085c248EeA27D92E8b30b2C58ed07f9E20001","tokenName":"USD Coin","tokenVersion":"2","supportsEip2612":true,"supportsEip7702":true}],"wallets":[{"blockchain":"BASE","address":"merchant","feePayer":"payer"}],"paymentMethods":[{"scheme":"balance","network":"inflow:1","payTo":"seller","decimals":18,"extra":{"future":1,"assetName":"wrong"}}],"supported":[]}),
        supported: json!({"kinds":[{"x402Version":2,"scheme":"exact","network":"eip155:8453","extra":{"supportsEip7702":true}}],"extensions":["eip2612GasSponsoring","inflowEip7702GasSponsoring"],"signers":{"eip155:8453":["exact"],"eip155:*":["wildcard"]}}),
        ..Default::default()
    })))
}

#[tokio::test]
async fn shared_seller_offer_and_sponsorship_vectors() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("data/offers.json")).unwrap();
    assert_eq!(cases.len(), 19);
    let token = CancellationToken::new();
    for case in cases {
        let server = fixture();
        {
            let mut state = server.0.lock().unwrap();
            state.config = case["input"]["config"].clone();
            if let Some(supported) = case["input"].get("supported") {
                state.supported = supported.clone();
            }
        }
        let seller = seller(&server).await;
        let input = &case["input"]["options"];
        let mut options = OfferOptions::new(input["price"].as_str().unwrap());
        options.schemes = input
            .get("schemes")
            .map(|v| serde_json::from_value(v.clone()).unwrap());
        options.networks = input
            .get("networks")
            .map(|v| serde_json::from_value(v.clone()).unwrap());
        options.permit2 = input["assetTransferMethod"] == "permit2";
        let result = if case["operation"] == "x402.seller.route" {
            seller.route(&options, &token).await.map(|r| {
                let mut value = json!({"accepts":normalized(r.accepts)});
                if !r.extensions.is_empty() {
                    value["extensions"] = json!(r.extensions);
                }
                value
            })
        } else {
            seller.offers(&options, &token).await.map(normalized)
        };
        if case["expect"].get("error").is_some() {
            assert!(result.is_err(), "{}", case["id"]);
        } else {
            assert_eq!(result.unwrap(), case["expect"]["result"], "{}", case["id"]);
        }
    }
}

fn normalized(offers: Vec<PaymentRequirements>) -> Value {
    json!(offers.into_iter().map(|v| json!({"scheme":v.scheme,"network":v.network,"payTo":v.pay_to,"price":{"asset":v.asset,"amount":v.amount},"maxTimeoutSeconds":v.max_timeout_seconds,"extra":v.extra})).collect::<Vec<_>>())
}

#[tokio::test]
async fn instrument_offers_require_opt_in_fiat_and_exact_bounded_cents() {
    for (price, schemes, network, empty_assets, expected) in [
        ("$1", None, None, false, Some(0)),
        ("1 USDC", Some("instrument"), None, false, Some(0)),
        (
            "$1",
            Some("instrument"),
            Some("inflow:other"),
            false,
            Some(0),
        ),
        ("$1", Some("instrument"), None, true, Some(1)),
        ("$0.50", Some("instrument"), None, false, Some(1)),
        ("$1.000", Some("instrument"), None, false, Some(1)),
        (
            "$92233720368547758.07",
            Some("instrument"),
            None,
            false,
            Some(1),
        ),
        ("$0.49", Some("instrument"), None, false, None),
        ("$1.001", Some("instrument"), None, false, None),
        (
            "$92233720368547758.08",
            Some("instrument"),
            None,
            false,
            None,
        ),
    ] {
        let server = fixture();
        {
            let mut state = server.0.lock().unwrap();
            state.config["paymentMethods"] = json!([{"scheme":"instrument","network":"inflow:1","payTo":"seller","decimals":18}]);
            if empty_assets {
                state.config["assets"] = json!([]);
                state.config["wallets"] = json!([]);
            }
        }
        let seller = seller(&server).await;
        let mut options = OfferOptions::new(price);
        options.schemes = schemes.map(|v| vec![v.into()]);
        options.networks = network.map(|v| vec![v.into()]);
        let result = seller.offers(&options, &CancellationToken::new()).await;
        match expected {
            None => assert!(result.is_err(), "{price}"),
            Some(count) => {
                let offers = normalized(result.unwrap());
                let instruments: Vec<_> = offers
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|v| v["scheme"] == "instrument")
                    .collect();
                assert_eq!(instruments.len(), count, "{price}");
                for offer in instruments {
                    assert_eq!(offer["price"]["asset"], "USD");
                    assert_eq!(offer["extra"]["assetName"], "USD");
                    assert!(
                        offer["price"]["amount"]
                            .as_str()
                            .unwrap()
                            .ends_with("0000000000000000")
                    );
                }
            }
        }
    }
}
fn options(server: &Server) -> ClientOptions {
    ClientOptions {
        authentication: Authentication::ApiKey("seller-secret".into()),
        transport: Some(Arc::new(server.clone())),
        ..Default::default()
    }
}
async fn seller(server: &Server) -> Seller {
    Seller::new(options(server), &CancellationToken::new())
        .await
        .unwrap()
}

struct SellerKey;
impl inflow_core::ApiKeyProvider for SellerKey {
    fn api_key(
        &self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, inflow_core::Error>> + Send + '_>,
    > {
        Box::pin(async { Ok("seller-secret".into()) })
    }
}

#[tokio::test]
async fn seller_accepts_api_key_provider() {
    let server = fixture();
    let mut options = options(&server);
    options.authentication = Authentication::ApiKeyProvider(Arc::new(SellerKey));
    Seller::new(options, &CancellationToken::new())
        .await
        .unwrap();
    let state = server.0.lock().unwrap();
    assert_eq!(state.requests.len(), 2);
    for request in &state.requests {
        assert_eq!(request.headers["x-api-key"], "seller-secret");
    }
}

#[tokio::test(start_paused = true)]
async fn construction_cache_refresh_and_facilitator_boundary() {
    let server = fixture();
    let token = CancellationToken::new();
    let seller = seller(&server).await;
    assert_eq!(server.0.lock().unwrap().requests.len(), 2);
    let before = seller.config(&token).await.unwrap();
    let mut copy = before.clone();
    copy["assets"] = json!([]);
    assert_eq!(seller.config(&token).await.unwrap(), before);
    let (a, b) = tokio::join!(seller.refresh_config(&token), seller.refresh_config(&token));
    assert_eq!(a.unwrap(), b.unwrap());
    assert_eq!(server.0.lock().unwrap().requests.len(), 3);
    seller.refresh_supported(&token).await.unwrap();
    assert_eq!(
        seller
            .signer_addresses("eip155:8453", &token)
            .await
            .unwrap(),
        ["exact"]
    );
    assert_eq!(
        seller.signer_addresses("eip155:1", &token).await.unwrap(),
        ["wildcard"]
    );
    for network in ["missing", ":bad", "solana:1"] {
        assert!(
            seller
                .signer_addresses(network, &token)
                .await
                .unwrap()
                .is_empty()
        );
    }
    tokio::time::advance(Duration::from_secs(3600)).await;
    seller.config(&token).await.unwrap();
    seller.facilitator().supported(&token).await.unwrap();
    assert_eq!(server.0.lock().unwrap().requests.len(), 6);
    server.0.lock().unwrap().fail = true;
    assert!(seller.refresh_config(&token).await.is_err());
    assert_eq!(seller.config(&token).await.unwrap(), before);
    server.0.lock().unwrap().fail = false;
    let request=inflow_x402::facilitator_request(&json!({"x402Version":2,"accepted":{},"payload":{"transactionId":"tx"},"extensions":{"future":{"keep":true}}}),&json!({})).unwrap();
    let facilitator = seller.facilitator();
    assert!(
        facilitator.verify(&request, &token).await.unwrap().0["isValid"]
            .as_bool()
            .unwrap()
    );
    assert!(
        facilitator.settle(&request, &token).await.unwrap().0["success"]
            .as_bool()
            .unwrap()
    );
    assert!(
        !<Facilitator as x402_types::facilitator::Facilitator>::supported(&facilitator)
            .await
            .unwrap()
            .kinds
            .is_empty()
    );
    <Facilitator as x402_types::facilitator::Facilitator>::verify(&facilitator, &request)
        .await
        .unwrap();
    <Facilitator as x402_types::facilitator::Facilitator>::settle(&facilitator, &request)
        .await
        .unwrap();
    let s = server.0.lock().unwrap();
    for request in &s.requests {
        assert_eq!(request.headers["x-api-key"], "seller-secret");
    }
    let bodies: Vec<Value> = s
        .requests
        .iter()
        .filter(|r| !r.body.is_empty())
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(bodies[0], bodies[1]);
    assert_eq!(
        bodies[0]["paymentPayload"]["extensions"]["future"]["keep"],
        true
    );
}

#[tokio::test]
async fn configuration_failures_cancellation_and_anonymous_facilitator() {
    let server = fixture();
    let token = CancellationToken::new();
    for authentication in [
        Authentication::Anonymous,
        Authentication::ApiKey(" ".into()),
    ] {
        let mut o = options(&server);
        o.authentication = authentication;
        assert!(Seller::new(o, &token).await.is_err());
    }
    let mut o = options(&server);
    o.authentication = Authentication::ApiKey("bad\nkey".into());
    assert!(Facilitator::new(o).is_err());
    let mut o = options(&server);
    o.authentication = Authentication::Anonymous;
    let f = Facilitator::new(o).unwrap();
    f.supported(&token).await.unwrap();
    assert!(
        !server.0.lock().unwrap().requests[0]
            .headers
            .contains_key("x-api-key")
    );
    token.cancel();
    assert_eq!(f.supported(&token).await.unwrap_err().code, "CANCELLED");
    for field in ["assets", "wallets", "paymentMethods"] {
        let s = fixture();
        s.0.lock().unwrap().config[field] = Value::Null;
        assert!(
            Seller::new(options(&s), &CancellationToken::new())
                .await
                .is_err()
        );
    }
    server.0.lock().unwrap().supported = json!({"kinds":"invalid"});
    assert!(
        Seller::new(options(&server), &CancellationToken::new())
            .await
            .is_err()
    );
    server.0.lock().unwrap().fail = true;
    assert!(
        Seller::new(options(&server), &CancellationToken::new())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn prices_filters_and_metadata_match_node() {
    let server = fixture();
    let seller = seller(&server).await;
    let t = CancellationToken::new();
    for (price, amount) in [
        ("$0.01", "10000"),
        ("1 USDC", "1000000"),
        ("0001.00000000 USDC", "1000000"),
        ("$9007199254740993.12", "9007199254740993120000"),
        ("$0", "0"),
    ] {
        let offers = seller.offers(&OfferOptions::new(price), &t).await.unwrap();
        assert_eq!(offers.len(), 2);
        assert_eq!(offers[0].amount, amount);
        assert_eq!(offers[1].extra.as_ref().unwrap()["assetName"], "USDC");
        assert_eq!(offers[1].extra.as_ref().unwrap()["future"], 1);
        assert_eq!(offers[0].extra.as_ref().unwrap()["feePayer"], "payer");
    }
    for price in [
        "",
        "one",
        "1",
        "-1 USDC",
        "$1.",
        "$.1",
        "$1.123456789",
        "1 usd",
        "1 USD extra",
        "1.0000001 USDC",
        "1e3 USD",
        " $1",
        "$1 ",
    ] {
        assert!(
            seller.offers(&OfferOptions::new(price), &t).await.is_err(),
            "{price}"
        );
    }
    server.0.lock().unwrap().config["paymentMethods"][0]["extra"] = Value::Null;
    seller.refresh_config(&t).await.unwrap();
    let offers = seller.offers(&OfferOptions::new("$1"), &t).await.unwrap();
    assert_eq!(offers[1].extra.as_ref().unwrap()["assetName"], "USDC");
    let mut o = OfferOptions::new(Price {
        amount: "$1".into(),
        currency: Some("USDT".into()),
    });
    let offers = seller.offers(&o, &t).await.unwrap();
    assert_eq!(offers.len(), 1);
    assert_eq!(offers[0].asset, "USDT");
    o.price = "$1".into();
    o.schemes = Some(vec!["exact".into()]);
    o.networks = Some(vec!["inflow:1".into()]);
    assert!(seller.offers(&o, &t).await.unwrap().is_empty());
    o.networks = Some(vec!["eip155:8453".into()]);
    o.permit2 = true;
    assert_eq!(seller.offers(&o, &t).await.unwrap().len(), 1);
    server.0.lock().unwrap().config["assets"][0]["permit2Proxy"] = json!("different");
    seller.refresh_config(&t).await.unwrap();
    assert!(seller.offers(&o, &t).await.unwrap().is_empty());
    server.0.lock().unwrap().config["assets"][0]["network"] = json!("invalid");
    o.permit2 = false;
    o.networks = None;
    seller.refresh_config(&t).await.unwrap();
    assert!(seller.offers(&o, &t).await.is_err());
    server.0.lock().unwrap().config["assets"][0]["decimals"] = json!(-1);
    seller.refresh_config(&t).await.unwrap();
    assert!(seller.offers(&o, &t).await.is_err());
}

#[tokio::test]
async fn sponsorship_requires_asset_and_facilitator_capabilities() {
    let t = CancellationToken::new();
    for (key, value, expected) in [
        ("supportsEip2612", json!(true), "eip2612GasSponsoring"),
        (
            "supportsEip2612",
            json!(false),
            "inflowEip7702GasSponsoring",
        ),
        ("tokenName", json!(""), "inflowEip7702GasSponsoring"),
        ("permit2Proxy", json!("wrong"), ""),
        ("assetTransferMethod", json!("eip3009"), ""),
    ] {
        let s = fixture();
        s.0.lock().unwrap().config["assets"][0][key] = value;
        let seller = seller(&s).await;
        let route = seller.route(&OfferOptions::new("$1"), &t).await.unwrap();
        if expected.is_empty() {
            assert!(route.extensions.is_empty());
        } else {
            assert_eq!(route.extensions.len(), 1);
            assert_eq!(route.extensions[expected]["info"]["version"], "1");
        }
    }
    for field in ["extensions", "kinds"] {
        let s = fixture();
        s.0.lock().unwrap().supported[field] = json!([]);
        let seller = seller(&s).await;
        assert!(
            seller
                .route(&OfferOptions::new("$1"), &t)
                .await
                .unwrap()
                .extensions
                .is_empty()
        );
    }
    let s = fixture();
    {
        let mut state = s.0.lock().unwrap();
        state.config["assets"][0]["supportsEip2612"] = json!(false);
        state.config["assets"][0]["supportsEip7702"] = json!(false);
    }
    let seller = seller(&s).await;
    assert!(
        seller
            .route(&OfferOptions::new("$1"), &t)
            .await
            .unwrap()
            .extensions
            .is_empty()
    );
    let o = OfferOptions {
        schemes: Some(vec![]),
        ..OfferOptions::new("$1")
    };
    assert!(seller.route(&o, &t).await.unwrap().accepts.is_empty());
}

#[tokio::test]
async fn metered_offers_are_explicit_and_require_complete_metadata() {
    let s = fixture();
    s.0.lock().unwrap().config["supported"] = json!([{"scheme":"upto","network":"eip155:8453","x402Version":2,"extra":{"assetTransferMethod":"permit2","permit2Proxy":"metered-proxy","facilitatorAddress":"witness"}}]);
    let seller = seller(&s).await;
    let t = CancellationToken::new();
    let mut o = OfferOptions::new("$1");
    assert!(
        seller
            .offers(&o, &t)
            .await
            .unwrap()
            .iter()
            .all(|v| v.scheme != "upto")
    );
    o.schemes = Some(vec!["upto".into()]);
    let offers = seller.offers(&o, &t).await.unwrap();
    assert_eq!(offers.len(), 1);
    assert_eq!(
        offers[0].extra.as_ref().unwrap()["permit2Proxy"],
        "metered-proxy"
    );
    s.0.lock().unwrap().config["supported"][0]["extra"]["facilitatorAddress"] = json!("");
    seller.refresh_config(&t).await.unwrap();
    assert!(seller.offers(&o, &t).await.unwrap().is_empty());
}

#[tokio::test]
async fn multi_chain_order_precision_and_open_payment_schemes() {
    let s = fixture();
    {
        let mut state = s.0.lock().unwrap();
        state.config["assets"].as_array_mut().unwrap().push(json!({"blockchain":"SOLANA","currency":"USDC","assetName":"USDC","assetId":"mint","decimals":6,"network":"solana:mainnet","assetTransferMethod":"solana"}));
        state.config["assets"].as_array_mut().unwrap().push(json!({"blockchain":"OTHER","currency":"USDT","assetName":"USDT","assetId":"other","decimals":6,"network":"eip155:1"}));
        state.config["wallets"]
            .as_array_mut()
            .unwrap()
            .push(json!({"blockchain":"SOLANA","address":"solana-wallet","feePayer":"sponsor"}));
        state.config["paymentMethods"][0]["scheme"] = json!("future");
    }
    let seller = seller(&s).await;
    let t = CancellationToken::new();
    let mut o = OfferOptions::new(Price {
        amount: "1.5".into(),
        currency: Some("USD".into()),
    });
    o.max_timeout_seconds = 42;
    let offers = seller.offers(&o, &t).await.unwrap();
    assert_eq!(offers.len(), 4);
    assert_eq!(offers[1].pay_to, "solana-wallet");
    assert_eq!(offers[1].extra.as_ref().unwrap()["feePayer"], "sponsor");
    assert_eq!(offers[1].extra.as_ref().unwrap()["name"], Value::Null);
    assert_eq!(offers[2].scheme, "future");
    assert_eq!(offers[3].asset, "USDT");
    assert!(offers.iter().all(|v| v.max_timeout_seconds == 42));
    o.permit2 = true;
    assert_eq!(seller.offers(&o, &t).await.unwrap().len(), 3);
    s.0.lock().unwrap().config["paymentMethods"][0]["decimals"] = json!(0);
    seller.refresh_config(&t).await.unwrap();
    assert!(seller.offers(&o, &t).await.is_err());
    o.price = "$1".into();
    s.0.lock().unwrap().config["paymentMethods"][0]["network"] = json!("invalid");
    seller.refresh_config(&t).await.unwrap();
    assert!(seller.offers(&o, &t).await.is_err());
}

#[tokio::test]
async fn malformed_or_failed_refresh_is_not_used_to_advertise_a_route() {
    let s = fixture();
    let seller = seller(&s).await;
    let t = CancellationToken::new();
    for currency in ["USDC1", "US_D", "U$"] {
        let result = seller
            .offers(&OfferOptions::new(format!("1 {currency}").as_str()), &t)
            .await;
        assert_eq!(result.is_ok(), currency != "U$");
    }
    s.0.lock().unwrap().fail = true;
    assert!(seller.route(&OfferOptions::new("$1"), &t).await.is_err());
    assert!(seller.signer_addresses("eip155:1", &t).await.is_ok());
    s.0.lock().unwrap().fail = false;
    s.0.lock().unwrap().supported = json!({"kinds":false});
    assert!(seller.refresh_supported(&t).await.is_err());
    let token = CancellationToken::new();
    token.cancel();
    assert!(
        seller
            .route(&OfferOptions::new("$1"), &token)
            .await
            .is_err()
    );
    assert!(seller.signer_addresses("eip155:1", &token).await.is_err());
    let mut bad = options(&s);
    bad.authentication = Authentication::ApiKey("bad\rkey".into());
    assert!(Seller::new(bad, &t).await.is_err());
}
