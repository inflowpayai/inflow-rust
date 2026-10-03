#![cfg(feature = "evm")]

use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use inflow_core::{Transport, TransportError, TransportRequest, TransportResponse};
use inflow_x402_buyer::{eip7702::*, *};
use serde_json::{Value, json};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/eip7702-node.json")).unwrap()
}
fn required() -> PaymentRequired<OriginalJson> {
    serde_json::from_value(
        json!({"x402Version":2,"accepts":[],"extensions":{EXTENSION:{"info":{"version":"1"}}}}),
    )
    .unwrap()
}

struct Wallet {
    allowance: U256,
    consent: bool,
    wrong: bool,
    calls: AtomicUsize,
    fail: AtomicUsize,
}
impl Wallet {
    fn key(&self) -> PrivateKeySigner {
        // Public disposable fixture key; these tests neither fund wallets nor broadcast transactions.
        (if self.wrong { "02" } else { "01" })
            .repeat(32)
            .parse()
            .unwrap()
    }
}
impl eip7702::Signer for Wallet {
    fn address(&self) -> Address {
        fixture()["owner"].as_str().unwrap().parse().unwrap()
    }
    fn allowance(
        &self,
        token: Address,
        owner: Address,
        spender: Address,
    ) -> WalletFuture<'_, U256> {
        assert_eq!(
            token.to_string().to_lowercase(),
            fixture()["payload"]["accepted"]["asset"]
                .as_str()
                .unwrap()
                .to_lowercase()
        );
        assert_eq!(owner, self.address());
        assert_eq!(
            spender.to_string().to_lowercase(),
            "0x000000000022d473030f116ddee9f6b43ac78ba3"
        );
        Box::pin(async move {
            if self.fail.load(Ordering::SeqCst) == 1 {
                Err(Error::new("WALLET_FAILED", "allowance unavailable"))
            } else {
                Ok(self.allowance)
            }
        })
    }
    fn sign_message(&self, hash: B256) -> WalletFuture<'_, Signature> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if self.fail.load(Ordering::SeqCst) == 4 {
                return Err(Error::new("WALLET_FAILED", "message declined"));
            }
            Ok(self.key().sign_message_sync(hash.as_slice()).unwrap())
        })
    }
    fn sign_authorization(&self, authorization: Authorization) -> WalletFuture<'_, Signature> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if self.fail.load(Ordering::SeqCst) == 3 {
                return Err(Error::new("WALLET_FAILED", "authorization declined"));
            }
            Ok(self
                .key()
                .sign_hash_sync(&authorization.signature_hash())
                .unwrap())
        })
    }
}
impl DelegationConsent for Wallet {
    fn approve(&self, _: Authorization) -> WalletFuture<'_, bool> {
        Box::pin(async move {
            if self.fail.load(Ordering::SeqCst) == 2 {
                Err(Error::new("WALLET_FAILED", "consent unavailable"))
            } else {
                Ok(self.consent)
            }
        })
    }
}
struct Platform {
    response: Value,
    requests: Mutex<Vec<TransportRequest>>,
    status: u16,
}
impl Transport for Platform {
    fn send(
        &self,
        request: TransportRequest,
    ) -> Pin<Box<dyn Future<Output = Result<TransportResponse, TransportError>> + Send + '_>> {
        self.requests.lock().unwrap().push(request);
        Box::pin(async move {
            Ok(TransportResponse {
                status: self.status,
                headers: Default::default(),
                body: self.response.to_string().into_bytes(),
            })
        })
    }
}
fn setup(
    prepared: Value,
    allowance: u64,
    consent: bool,
    wrong: bool,
    status: u16,
) -> (Sponsorship, Arc<Wallet>, Arc<Platform>) {
    let platform = Arc::new(Platform {
        response: prepared,
        requests: Default::default(),
        status,
    });
    let wallet = Arc::new(Wallet {
        allowance: U256::from(allowance),
        consent,
        wrong,
        calls: AtomicUsize::new(0),
        fail: AtomicUsize::new(0),
    });
    let extension = Sponsorship::new(
        ClientOptions {
            transport: Some(platform.clone()),
            ..Default::default()
        },
        wallet.clone(),
        wallet.clone(),
    )
    .unwrap();
    (extension, wallet, platform)
}

#[tokio::test]
async fn canonical_node_operation_is_signed_without_broadcast_or_credentials() {
    for authorize in [true, false] {
        let f = fixture();
        let mut prepared = f["prepared"].clone();
        if !authorize {
            prepared.as_object_mut().unwrap().remove("authorization");
        }
        let (extension, wallet, platform) = setup(prepared, 0, true, false, 200);
        let original = f["payload"].clone();
        let result = extension
            .enrich(original.clone(), &required(), &CancellationToken::new())
            .await
            .unwrap();
        let info = &result["extensions"][EXTENSION]["info"];
        assert_eq!(info["sponsorshipId"], f["prepared"]["sponsorshipId"]);
        let sig: Signature = info["signature"].as_str().unwrap().parse().unwrap();
        let hash: B256 = f["prepared"]["userOperationHash"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            sig.recover_address_from_msg(hash).unwrap(),
            wallet.address()
        );
        assert_eq!(info.get("authorizationSignature").is_some(), authorize);
        assert_eq!(
            wallet.calls.load(Ordering::SeqCst),
            if authorize { 2 } else { 1 }
        );
        assert_eq!(result["payload"], original["payload"]);
        let calls = platform.requests.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].url,
            "https://api.inflowpay.ai/v1/x402/eip7702/prepare"
        );
        assert!(!calls[0].headers.contains_key("authorization"));
        assert!(!calls[0].headers.contains_key("x-api-key"));
        let body: Value = serde_json::from_slice(&calls[0].body).unwrap();
        assert_eq!(body["paymentPayload"], original);
        assert_eq!(body["paymentRequirements"], original["accepted"]);
    }
}

#[tokio::test]
async fn skip_consent_decline_wrong_signatures_cancellation_and_http_failure() {
    let f = fixture();
    let token = CancellationToken::new();
    let (extension, wallet, platform) = setup(f["prepared"].clone(), 123, true, false, 200);
    assert_eq!(
        extension
            .enrich(f["payload"].clone(), &required(), &token)
            .await
            .unwrap(),
        f["payload"]
    );
    assert!(platform.requests.lock().unwrap().is_empty());
    let no_extension = serde_json::from_value(json!({"x402Version":2,"accepts":[]})).unwrap();
    extension
        .enrich(f["payload"].clone(), &no_extension, &token)
        .await
        .unwrap();
    for path in ["/accepted/scheme", "/accepted/extra/assetTransferMethod"] {
        let mut payment = f["payload"].clone();
        *payment.pointer_mut(path).unwrap() = json!("other");
        assert_eq!(
            extension
                .enrich(payment.clone(), &required(), &token)
                .await
                .unwrap(),
            payment
        );
    }
    token.cancel();
    assert_eq!(
        extension
            .enrich(f["payload"].clone(), &required(), &token)
            .await
            .unwrap_err()
            .code,
        "X402_APPROVAL_CANCELLED"
    );
    assert_eq!(wallet.calls.load(Ordering::SeqCst), 0);
    for (consent, wrong, status, authorize) in [
        (false, false, 200, true),
        (true, true, 200, true),
        (true, true, 200, false),
        (true, false, 503, true),
    ] {
        let mut prepared = f["prepared"].clone();
        if !authorize {
            prepared.as_object_mut().unwrap().remove("authorization");
        }
        let (extension, _, platform) = setup(prepared, 0, consent, wrong, status);
        assert!(
            extension
                .enrich(f["payload"].clone(), &required(), &CancellationToken::new())
                .await
                .is_err()
        );
        assert_eq!(platform.requests.lock().unwrap().len(), 1);
    }
    let (extension, wallet, _) = setup(f["prepared"].clone(), 0, true, false, 200);
    for declaration in [
        json!(null),
        json!({"info":null}),
        json!({"info":{"version":"2"}}),
        json!({"info":{"version":"1","url":"https://evil.example"}}),
        json!({"info":{"version":"1"},"url":"https://evil.example"}),
    ] {
        let r = serde_json::from_value(
            json!({"x402Version":2,"accepts":[],"extensions":{EXTENSION:declaration}}),
        )
        .unwrap();
        assert!(
            extension
                .enrich(f["payload"].clone(), &r, &CancellationToken::new())
                .await
                .is_err()
        );
    }
    assert_eq!(wallet.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn reject_tampered_payment_before_preparation_or_signing() {
    let f = fixture();
    let (extension, wallet, platform) = setup(f["prepared"].clone(), 0, true, false, 200);
    for (path, value) in [
        ("/x402Version", json!(1)),
        ("/accepted/network", json!("eip155:01")),
        ("/accepted/network", json!("eip155:9007199254740992")),
        ("/accepted/network", json!("eip155:x")),
        ("/accepted/amount", json!("124")),
        (
            "/accepted/asset",
            json!("0x0000000000000000000000000000000000000000"),
        ),
        ("/accepted/payTo", json!("invalid")),
        (
            "/accepted/extra/permit2Proxy",
            json!("0x0000000000000000000000000000000000000000"),
        ),
        ("/payload/signature", json!("0x11")),
        ("/payload/signature", json!("0xzz")),
        (
            "/payload/permit2Authorization/from",
            json!("0x0000000000000000000000000000000000000000"),
        ),
        (
            "/payload/permit2Authorization/spender",
            json!("0x0000000000000000000000000000000000000000"),
        ),
        (
            "/payload/permit2Authorization/witness/to",
            json!("0x0000000000000000000000000000000000000000"),
        ),
        ("/payload/permit2Authorization/deadline", json!("0")),
        ("/payload/permit2Authorization/nonce", json!("01")),
        ("/payload/permit2Authorization/nonce", json!("-1")),
        ("/payload/permit2Authorization/nonce", json!("9".repeat(79))),
        ("/payload/permit2Authorization/permitted/amount", json!("0")),
    ] {
        let mut payment = f["payload"].clone();
        *payment.pointer_mut(path).unwrap() = value;
        assert!(
            extension
                .enrich(payment, &required(), &CancellationToken::new())
                .await
                .is_err(),
            "{path}"
        );
    }
    assert_eq!(wallet.calls.load(Ordering::SeqCst), 0);
    assert!(platform.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reject_tampered_preparation_without_signing() {
    let f = fixture();
    for (path, value) in [
        ("/sponsorshipId", json!(null)),
        ("/sponsorshipId", json!("invalid")),
        ("/chainId", json!(1)),
        ("/chainId", json!(9007199254740992u64)),
        ("/entryPointVersion", json!("0.8")),
        (
            "/entryPoint",
            json!("0x0000000000000000000000000000000000000000"),
        ),
        (
            "/delegation",
            json!("0x0000000000000000000000000000000000000000"),
        ),
        (
            "/userOperation/sender",
            json!("0x0000000000000000000000000000000000000000"),
        ),
        ("/userOperation/callData", json!("0x")),
        ("/userOperation/nonce", json!("0x0")),
        ("/userOperation/callGasLimit", json!("0x0")),
        ("/userOperation/callGasLimit", json!("0x01")),
        (
            "/userOperation/callGasLimit",
            json!("0x100000000000000000000000000000000"),
        ),
        (
            "/userOperation/callGasLimit",
            json!("0x".to_owned() + &"f".repeat(65)),
        ),
        ("/userOperation/verificationGasLimit", json!("0x0")),
        (
            "/userOperation/paymaster",
            json!("0x1111111111111111111111111111111111111111"),
        ),
        ("/userOperation/paymasterData", json!("0x11")),
        ("/userOperation/maxFeePerGas", json!("0x1")),
        ("/userOperationHash", json!("0x")),
        ("/expiresAt", json!(0)),
        ("/expiresAt", json!(4102444801u64)),
        ("/authorization", json!(null)),
        ("/authorization/chainId", json!(1)),
        (
            "/authorization/address",
            json!("0x0000000000000000000000000000000000000000"),
        ),
        ("/authorization/nonce", json!(-1)),
        ("/userOperation", json!({"factory":"0x123"})),
    ] {
        let mut prepared = f["prepared"].clone();
        *prepared.pointer_mut(path).unwrap() = value;
        let (extension, wallet, _) = setup(prepared, 0, true, false, 200);
        assert!(
            extension
                .enrich(f["payload"].clone(), &required(), &CancellationToken::new())
                .await
                .is_err(),
            "{path}"
        );
        assert_eq!(wallet.calls.load(Ordering::SeqCst), 0, "{path}");
    }
}

#[tokio::test]
async fn actual_upstream_exact_signer_omits_only_unsigned_eip2612_advertisement() {
    let signer: PrivateKeySigner = "01".repeat(32).parse().unwrap();
    let f = fixture();
    let mut offer = f["payload"]["accepted"].clone();
    // Rust upstream requires token domain metadata even for Permit2, unlike the Node fixture.
    offer["extra"]["name"] = json!("USD Coin");
    offer["extra"]["version"] = json!("2");
    let r=serde_json::from_value(json!({"x402Version":2,"accepts":[offer],"extensions":{"eip2612GasSponsoring":{"info":{"version":"1"}}}})).unwrap();
    let buyer = HttpBuyer::new(None)
        .unwrap()
        .register(evm::V2Eip155ExactClient::new(signer));
    let result = buyer
        .payment(
            &r,
            SignOptions::default(),
            WaitOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        result.payment_payload["payload"]["signature"]
            .as_str()
            .is_some()
    );
    assert!(result.payment_payload["payload"]["permit2Authorization"].is_object());
    assert!(
        result.payment_payload["extensions"]
            .get("eip2612GasSponsoring")
            .is_none()
    );
}

#[tokio::test]
async fn missing_fields_and_wallet_errors_do_not_produce_a_partial_credential() {
    let f = fixture();
    for path in [
        "/accepted/network",
        "/accepted/asset",
        "/accepted/amount",
        "/accepted/payTo",
        "/accepted/extra/permit2Proxy",
        "/payload/permit2Authorization/permitted/token",
        "/payload/permit2Authorization/permitted/amount",
        "/payload/permit2Authorization/from",
        "/payload/permit2Authorization/spender",
        "/payload/permit2Authorization/deadline",
        "/payload/permit2Authorization/nonce",
        "/payload/permit2Authorization/witness/to",
        "/payload/permit2Authorization/witness/validAfter",
        "/payload/signature",
    ] {
        let (extension, wallet, platform) = setup(f["prepared"].clone(), 0, true, false, 200);
        let mut payment = f["payload"].clone();
        *payment.pointer_mut(path).unwrap() = Value::Null;
        assert!(
            extension
                .enrich(payment, &required(), &CancellationToken::new())
                .await
                .is_err(),
            "{path}"
        );
        assert_eq!(wallet.calls.load(Ordering::SeqCst), 0);
        assert!(platform.requests.lock().unwrap().is_empty());
    }
    for path in [
        "/userOperation/nonce",
        "/userOperation/callData",
        "/userOperation/callGasLimit",
        "/userOperation/verificationGasLimit",
        "/userOperation/sender",
        "/userOperation/paymaster",
        "/userOperationHash",
        "/entryPoint",
        "/delegation",
        "/authorization/chainId",
        "/authorization/address",
    ] {
        let mut prepared = f["prepared"].clone();
        *prepared.pointer_mut(path).unwrap() = Value::Null;
        let (extension, wallet, _) = setup(prepared, 0, true, false, 200);
        assert!(
            extension
                .enrich(f["payload"].clone(), &required(), &CancellationToken::new())
                .await
                .is_err(),
            "{path}"
        );
        assert_eq!(wallet.calls.load(Ordering::SeqCst), 0);
    }
    for fail in 1..=4 {
        let (extension, wallet, _) = setup(f["prepared"].clone(), 0, true, false, 200);
        wallet.fail.store(fail, Ordering::SeqCst);
        assert_eq!(
            extension
                .enrich(f["payload"].clone(), &required(), &CancellationToken::new())
                .await
                .unwrap_err()
                .code,
            "WALLET_FAILED"
        );
    }
    let (extension, _, _) = setup(f["prepared"].clone(), 0, true, false, 200);
    let mut payment = f["payload"].clone();
    payment["extensions"] = Value::Null;
    assert!(
        extension
            .enrich(payment, &required(), &CancellationToken::new())
            .await
            .is_err()
    );
    let wallet = Arc::new(Wallet {
        allowance: U256::ZERO,
        consent: true,
        wrong: false,
        calls: AtomicUsize::new(0),
        fail: AtomicUsize::new(0),
    });
    assert!(
        Sponsorship::new(
            ClientOptions {
                timeout: std::time::Duration::ZERO,
                ..Default::default()
            },
            wallet.clone(),
            wallet
        )
        .is_err()
    );
}
