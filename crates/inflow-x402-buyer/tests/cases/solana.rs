use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, VerifyingKey};
use inflow_x402_buyer::*;
use serde_json::{Value, json};
use solana_client::{nonblocking::rpc_client::RpcClient, rpc_request::RpcRequest};
use solana_keypair::Keypair;
use solana_signer::Signer;
use std::{collections::HashMap, sync::Arc};

fn rpc() -> Arc<RpcClient> {
    // Only chain reads and simulation are fixtures; upstream builds and signs the transaction.
    let mut mint = [0_u8; 82];
    mint[44] = 6;
    mint[45] = 1;
    Arc::new(RpcClient::new_mock_with_mocks(
        "succeeds".into(),
        HashMap::from([
            (
                RpcRequest::GetAccountInfo,
                json!({"context":{"slot":1},"value":{
                    "data":[STANDARD.encode(mint),"base64"],"executable":false,
                    "lamports":1,"owner":"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA","rentEpoch":0
                }}),
            ),
            (
                RpcRequest::SimulateTransaction,
                json!({"context":{"slot":1},"value":{
                    "err":null,"logs":[],"unitsConsumed":20000
                }}),
            ),
        ]),
    ))
}

async fn sign(versions: Option<Value>) -> (Value, Vec<u8>, [u8; 32]) {
    let signer = Arc::new(Keypair::new_from_array([7; 32]));
    let public_key = signer.pubkey().to_bytes();
    let mut extra = json!({"feePayer":"11111111111111111111111111111111"});
    if let Some(versions) = versions {
        extra["transactionVersions"] = versions;
    }
    let offer = json!({"scheme":"exact","network":"solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
        "asset":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","amount":"1000",
        "payTo":"So11111111111111111111111111111111111111112","maxTimeoutSeconds":60,"extra":extra});
    let required: PaymentRequired<OriginalJson> = serde_json::from_value(json!({
        "x402Version":2,"resource":{"url":"https://merchant.example"},"accepts":[offer.clone()]
    }))
    .unwrap();
    let payment = HttpBuyer::new(None)
        .unwrap()
        .register(solana::V2SolanaExactClient::new(signer, rpc()))
        .payment(
            &required,
            SignOptions::default(),
            WaitOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(payment.payment_payload["accepted"], offer);
    let transaction = STANDARD
        .decode(
            payment.payment_payload["payload"]["transaction"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
    (payment.payment_payload, transaction, public_key)
}

#[tokio::test]
async fn native_solana_signer_produces_verifiable_version_zero_transactions() {
    for versions in [None, Some(json!([0]))] {
        let (_, transaction, public_key) = sign(versions).await;
        // Two signatures: the facilitator fee payer slot stays empty; the buyer signs slot one.
        assert_eq!(transaction[0], 2);
        assert!(transaction[1..65].iter().all(|byte| *byte == 0));
        let message = &transaction[129..];
        assert_eq!(message[0], 0x80);
        VerifyingKey::from_bytes(&public_key)
            .unwrap()
            .verify_strict(
                message,
                &Signature::from_slice(&transaction[65..129]).unwrap(),
            )
            .unwrap();
    }
}
