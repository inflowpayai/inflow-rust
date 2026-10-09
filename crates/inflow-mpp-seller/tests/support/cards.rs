use super::*;

fn configuration(method: Method) -> Value {
    let details = if method == Method::Card {
        json!({"recipient":"merchant", "merchantName":"Shop", "acceptedNetworks":["visa"],
            "encryptionJwk":{"kty":"RSA","alg":"RSA-OAEP-256","use":"enc","kid":"key","n":"abc","e":"AQAB"}})
    } else {
        json!({"networkId":"profile","paymentMethodTypes":["card","link"]})
    };
    json!({"sellerId":SELLER,"featureFlags":{"idempotencyKeyEnabled":true},"supportedMethods":[{
        "id":method.name(), "supportedCurrencies":["USD"],"supportedIntents":["charge"],"methodDetails":details}]})
}

#[tokio::test]
async fn exact_dollars_and_authoritative_configuration() {
    for method in [Method::Stripe, Method::Card] {
        let (seller, script) = seller(vec![Reply::Config(configuration(method))]).await;
        for (amount, cents) in [
            ("0.50", "50"),
            ("1", "100"),
            ("1.2", "120"),
            ("1.25", "125"),
            ("999999.99", "99999999"),
        ] {
            let input = json!({"amount":amount,"currency":"EUR","recipient":"override","decimals":9,"methodDetails":{},"networkId":"override","paymentMethodTypes":[],"billingRequired":false,"metadata":{"order":""},"externalId":"","description":"Read access"});
            let offer = seller
                .offer(method, input.clone(), Default::default())
                .unwrap();
            assert_eq!(offer.request()["amount"], cents);
            assert_eq!(offer.request()["currency"], "usd");
            assert_eq!(offer.request()["externalId"], "");
            assert_eq!(offer.request()["description"], "Read access");
            assert!(offer.request().get("decimals").is_none());
            assert_eq!(input["currency"], "EUR");
            if method == Method::Card {
                assert_eq!(offer.request()["recipient"], "merchant");
                assert_eq!(offer.request()["methodDetails"]["billingRequired"], false);
            } else {
                assert_eq!(offer.request()["methodDetails"]["networkId"], "profile");
                assert_eq!(
                    offer.request()["methodDetails"]["metadata"],
                    json!({"order":""})
                );
            }
        }
        for amount in [
            json!(null),
            json!(1.25),
            json!(""),
            json!("01"),
            json!(".5"),
            json!("1."),
            json!("1.000"),
            json!("1e2"),
            json!("-1"),
            json!("1.x"),
            json!("1000000"),
            json!("0.49"),
        ] {
            assert!(
                seller
                    .offer(method, json!({"amount":amount}), Default::default())
                    .is_err(),
                "{amount}"
            );
        }
        let basic = seller
            .offer(method, json!({"amount":"1"}), Default::default())
            .unwrap();
        assert!(
            basic.request()["methodDetails"]
                .get("billingRequired")
                .is_none()
        );
        assert_eq!(script.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn card_configuration_billing_default_and_route_override() {
    for billing in [json!(true), json!(false), json!("invalid")] {
        let mut config = configuration(Method::Card);
        config["supportedMethods"][0]["methodDetails"]["billingRequired"] = billing.clone();
        let (seller, _) = seller(vec![Reply::Config(config)]).await;
        let offer = seller.offer(Method::Card, json!({"amount":"1"}), Default::default());
        if billing.is_boolean() {
            assert_eq!(
                offer.unwrap().request()["methodDetails"]["billingRequired"],
                billing
            );
            let overridden = seller
                .offer(
                    Method::Card,
                    json!({"amount":"1","billingRequired":false}),
                    Default::default(),
                )
                .unwrap();
            assert_eq!(
                overridden.request()["methodDetails"]["billingRequired"],
                false
            );
        } else {
            assert_eq!(err(offer).code, "MPP_CARD_UNAVAILABLE");
        }
    }
}

#[tokio::test]
async fn unavailable_configuration_cannot_advertise_an_offer() {
    for method in [Method::Stripe, Method::Card] {
        for field in [
            "id",
            "supportedCurrencies",
            "supportedIntents",
            "methodDetails",
        ] {
            let mut config = configuration(method);
            config["supportedMethods"][0][field] = Value::Null;
            let (seller, _) = seller(vec![Reply::Config(config)]).await;
            assert_eq!(
                err(seller.offer(method, json!({"amount":"1"}), Default::default())).code,
                if method == Method::Stripe {
                    "MPP_STRIPE_UNAVAILABLE"
                } else {
                    "MPP_CARD_UNAVAILABLE"
                }
            );
        }
    }
}

#[tokio::test]
async fn card_and_stripe_validate_then_settle_and_require_bound_receipts() {
    for method in [Method::Stripe, Method::Card] {
        for outcome in ["success", "method", "challenge", "missing", "problem"] {
            let (seller, script) = seller(vec![
                Reply::Config(configuration(method)),
                Reply::Validate(json!({})),
            ])
            .await;
            let offer = seller
                .offer(
                    method,
                    json!({"amount":"1.25","externalId":"order"}),
                    Default::default(),
                )
                .unwrap();
            let mut c = credential(&offer, None);
            c.source = None;
            c.payload=if method==Method::Stripe {json!({"spt":"opaque-test-token","externalId":"order"})} else {json!({"encryptedPayload":"opaque","network":"visa","panLastFour":"1234","panExpirationMonth":"01","panExpirationYear":"2030"})}.as_object().unwrap().clone();
            let mut receipt = json!({"receipt":{"status":"success","method":method.name(),"challengeId":c.challenge.id,"reference":"ref","timestamp":"2026-10-08T00:00:00Z","externalId":"order"}});
            match outcome {
                "method" => receipt["receipt"]["method"] = json!("other"),
                "challenge" => receipt["receipt"]["challengeId"] = json!("other"),
                "missing" => {
                    receipt["receipt"]
                        .as_object_mut()
                        .unwrap()
                        .remove("challengeId");
                }
                "problem" => receipt = json!({"problem":{"detail":"pending"}}),
                _ => {}
            }
            script
                .replies
                .lock()
                .unwrap()
                .push_back(Reply::Receipt(receipt));
            let result = offer.accept(&c, None, &CancellationToken::new()).await;
            if outcome == "success" {
                assert_eq!(result.unwrap().reference, "ref");
            } else {
                let error = err(result);
                assert_eq!(error.code, "MPP_PAYMENT_FAILED");
                if outcome != "problem" {
                    assert_eq!(error.body["status"], 500);
                    assert_eq!(
                        error.body["type"],
                        "https://paymentauth.org/problems/internal-payment-error"
                    );
                }
            }
            let requests = script.requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
            assert_eq!(body["credential"]["source"], "");
            assert!(requests[1].url.ends_with("/validate"));
            assert!(requests[2].url.ends_with("/broadcast"));
        }
    }
}

#[tokio::test]
async fn card_terms_and_stripe_reference_cannot_be_replaced() {
    for method in [Method::Stripe, Method::Card] {
        let (seller, script) = seller(vec![Reply::Config(configuration(method))]).await;
        let first = seller
            .offer(
                method,
                json!({"amount":"1.25","externalId":"a"}),
                Default::default(),
            )
            .unwrap();
        let second = seller
            .offer(
                method,
                json!({"amount":"1.25","externalId":"b"}),
                Default::default(),
            )
            .unwrap();
        let c = credential(&first, None);
        assert_eq!(
            err(second.accept(&c, None, &CancellationToken::new()).await).code,
            "MPP_CREDENTIAL_MISMATCH"
        );
        if method == Method::Stripe {
            let mut c = c;
            c.payload = json!({"spt":"test","externalId":"b"})
                .as_object()
                .unwrap()
                .clone();
            assert_eq!(
                err(first.accept(&c, None, &CancellationToken::new()).await).code,
                "MPP_CREDENTIAL_MISMATCH"
            );
        }
        assert_eq!(script.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn card_and_stripe_reject_invalid_signatures_and_expiry_before_platform_calls() {
    for method in [Method::Card, Method::Stripe] {
        for failure in ["signature", "expired"] {
            let (seller, script) = seller(vec![Reply::Config(configuration(method))]).await;
            let offer = seller
                .offer(
                    method,
                    json!({"amount":"1.25"}),
                    ChallengeOptions {
                        description: Some("Original purchase".into()),
                        expires: Some(
                            if failure == "expired" {
                                "2000-01-01T00:00:00Z"
                            } else {
                                "2099-01-01T00:00:00Z"
                            }
                            .into(),
                        ),
                        ..Default::default()
                    },
                )
                .unwrap();
            let mut c = credential(&offer, None);
            c.payload = if method == Method::Stripe {
                json!({"spt":"test-token"})
            } else {
                json!({"encryptedPayload":"opaque","network":"visa","panLastFour":"1234","panExpirationMonth":"01","panExpirationYear":"2030"})
            }.as_object().unwrap().clone();
            if failure == "signature" {
                c.challenge.id.push('x');
            }
            assert!(
                offer
                    .accept(&c, None, &CancellationToken::new())
                    .await
                    .is_err(),
                "{failure}"
            );
            assert_eq!(script.requests.lock().unwrap().len(), 1);
        }
    }
}
