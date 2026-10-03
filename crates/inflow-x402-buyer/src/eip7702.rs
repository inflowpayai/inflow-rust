//! Opt-in external-wallet sponsorship. Prepares and signs an operation without broadcasting it.

use crate::{
    CancellationToken, ClientOptions, Error, OriginalJson, PaymentExtension, PaymentRequired,
};
pub use alloy_eip7702::Authorization;
pub use alloy_primitives::{Address, B256, Signature, U256};
use alloy_primitives::{Bytes, address, keccak256};
use alloy_sol_types::{SolCall, sol};
use inflow_core::internal::HttpClient;
use serde_json::{Value, json};
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

pub const EXTENSION: &str = "inflowEip7702GasSponsoring";
pub const DELEGATION: Address = address!("77021100bD87b7008E5E1989d0eB38555d0d0000");
pub const ENTRY_POINT: Address = address!("0000000071727De22E5E9d8BAf0edAc6f37da032");
const PERMIT2: Address = address!("000000000022D473030F116dDEE9F6B43aC78BA3");
const PROXY: Address = address!("402085c248EeA27D92E8b30b2C58ed07f9E20001");

pub type WalletFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

pub trait Signer: Send + Sync {
    fn address(&self) -> Address;
    fn allowance(&self, token: Address, owner: Address, spender: Address)
    -> WalletFuture<'_, U256>;
    /// Signs the hash as an Ethereum personal message, including its standard message prefix.
    fn sign_message(&self, hash: B256) -> WalletFuture<'_, Signature>;
    fn sign_authorization(&self, authorization: Authorization) -> WalletFuture<'_, Signature>;
}

pub trait DelegationConsent: Send + Sync {
    /// Delegation persists even when payment execution fails. Ask the wallet owner before returning true.
    fn approve(&self, authorization: Authorization) -> WalletFuture<'_, bool>;
}

pub struct Sponsorship {
    client: HttpClient,
    signer: Arc<dyn Signer>,
    consent: Arc<dyn DelegationConsent>,
}

impl Sponsorship {
    pub fn new(
        options: ClientOptions,
        signer: Arc<dyn Signer>,
        consent: Arc<dyn DelegationConsent>,
    ) -> Result<Self, Error> {
        Ok(Self {
            client: HttpClient::new(options)?,
            signer,
            consent,
        })
    }

    async fn apply(
        &self,
        mut payment: Value,
        required: &PaymentRequired<OriginalJson>,
        token: &CancellationToken,
    ) -> Result<Value, Error> {
        let declarations = required.extensions.as_ref();
        let Some(declaration) = declarations.get(EXTENSION) else {
            return Ok(payment);
        };
        if payment["accepted"]["scheme"] != "exact"
            || payment["accepted"]["extra"]["assetTransferMethod"] != "permit2"
        {
            return Ok(payment);
        }
        fields(declaration, &["info"])?;
        fields(&declaration["info"], &["version"])?;
        ensure(
            declaration["info"]["version"] == "1",
            "unsupported sponsorship version",
        )?;
        let owner = self.signer.address();
        let expected = payment_batch(&payment, owner)?;
        if let Some(extensions) = payment.get("extensions") {
            ensure(
                extensions.is_object(),
                "payment extensions must be an object",
            )?;
        }
        if self
            .signer
            .allowance(expected.asset, owner, PERMIT2)
            .await?
            >= expected.amount
        {
            return Ok(payment);
        }
        // Only the caller's configured platform can prepare delegation; merchant URLs are never used here.
        let prepared = self
            .client
            .request(
                ::http::Method::POST,
                "/v1/x402/eip7702/prepare",
                Some(json!({"paymentPayload":payment,"paymentRequirements":payment["accepted"]})),
                ::http::HeaderMap::new(),
                0,
                token,
            )
            .await?;
        let (hash, authorization) = validate_prepared(&prepared, &expected, owner)?;
        check_expiry(&prepared, expected.deadline)?;
        let mut info = json!({"version":"1", "sponsorshipId":prepared["sponsorshipId"]});
        if let Some(authorization) = authorization {
            ensure(
                self.consent.approve(authorization.clone()).await?,
                "delegation consent declined",
            )?;
            check_expiry(&prepared, expected.deadline)?;
            let signature = self
                .signer
                .sign_authorization(authorization.clone())
                .await?;
            ensure(
                signature
                    .recover_address_from_prehash(&authorization.signature_hash())
                    .ok()
                    == Some(owner),
                "delegation authorization has the wrong signer",
            )?;
            info["authorizationSignature"] = signature.to_string().into();
        }
        check_expiry(&prepared, expected.deadline)?;
        let signature = self.signer.sign_message(hash).await?;
        ensure(
            signature.recover_address_from_msg(hash).ok() == Some(owner),
            "operation has the wrong signer",
        )?;
        check_expiry(&prepared, expected.deadline)?;
        info["signature"] = signature.to_string().into();
        let extensions = payment
            .as_object_mut()
            .ok_or(invalid("payment object"))?
            .entry("extensions")
            .or_insert_with(|| json!({}));
        extensions
            .as_object_mut()
            .ok_or(invalid("payment extensions"))?
            .insert(EXTENSION.into(), json!({"info":info}));
        Ok(payment)
    }
}

impl PaymentExtension for Sponsorship {
    fn enrich<'a>(
        &'a self,
        payload: Value,
        required: &'a PaymentRequired<OriginalJson>,
        cancellation: &'a CancellationToken,
    ) -> WalletFuture<'a, Value> {
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(crate::buyer::cancelled()),
                result = self.apply(payload, required, cancellation) => result,
            }
        })
    }
}

sol! {
    struct TokenPermissions { address token; uint256 amount; }
    struct Permit { TokenPermissions permitted; uint256 nonce; uint256 deadline; }
    struct Witness { address to; uint256 validAfter; }
    struct Call { address target; uint256 value; bytes data; }
    function approve(address spender, uint256 amount) external returns (bool);
    function settle(Permit permit, address owner, Witness witness, bytes signature) external;
    function executeBatch(Call[] calls) external;
}

struct Expected {
    asset: Address,
    amount: U256,
    deadline: U256,
    chain: u64,
    call_data: Vec<u8>,
}

fn payment_batch(payment: &Value, owner: Address) -> Result<Expected, Error> {
    let r = &payment["accepted"];
    let chain = r["network"]
        .as_str()
        .and_then(|s| s.strip_prefix("eip155:"))
        .filter(|s| !s.starts_with('0') && s.bytes().all(|c| c.is_ascii_digit()))
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|v| *v > 0 && *v <= 9_007_199_254_740_991)
        .ok_or_else(|| invalid("payment network"))?;
    ensure(payment["x402Version"] == 2, "payment version")?;
    let a = &payment["payload"]["permit2Authorization"];
    let asset = addr(&a["permitted"]["token"])?;
    let amount = decimal(&a["permitted"]["amount"])?;
    let deadline = decimal(&a["deadline"])?;
    ensure(
        amount > U256::ZERO
            && amount == decimal(&r["amount"])?
            && asset == addr(&r["asset"])?
            && addr(&a["from"])? == owner
            && addr(&a["spender"])? == PROXY
            && addr(&r["extra"]["permit2Proxy"])? == PROXY
            && addr(&a["witness"]["to"])? == addr(&r["payTo"])?,
        "Permit2 authorization does not match payment",
    )?;
    ensure(deadline > U256::from(now()), "payment expired")?;
    let signature = bytes(&payment["payload"]["signature"])?;
    ensure(signature.len() == 65, "payment signature length")?;
    let settle = settleCall {
        permit: Permit {
            permitted: TokenPermissions {
                token: asset,
                amount,
            },
            nonce: decimal(&a["nonce"])?,
            deadline,
        },
        owner,
        witness: Witness {
            to: addr(&a["witness"]["to"])?,
            validAfter: decimal(&a["witness"]["validAfter"])?,
        },
        signature,
    }
    .abi_encode();
    let approve = approveCall {
        spender: PERMIT2,
        amount,
    }
    .abi_encode();
    let call_data = executeBatchCall {
        calls: vec![
            Call {
                target: asset,
                value: U256::ZERO,
                data: approve.into(),
            },
            Call {
                target: PROXY,
                value: U256::ZERO,
                data: settle.into(),
            },
        ],
    }
    .abi_encode();
    Ok(Expected {
        asset,
        amount,
        deadline,
        chain,
        call_data,
    })
}

fn validate_prepared(
    p: &Value,
    expected: &Expected,
    owner: Address,
) -> Result<(B256, Option<Authorization>), Error> {
    let id = p["sponsorshipId"]
        .as_str()
        .ok_or_else(|| invalid("sponsorship identifier"))?;
    ensure(
        id.len() == 36
            && id.bytes().enumerate().all(|(i, b)| {
                if [8, 13, 18, 23].contains(&i) {
                    b == b'-'
                } else {
                    b.is_ascii_hexdigit()
                }
            }),
        "sponsorship identifier",
    )?;
    ensure(
        integer(&p["chainId"])? == expected.chain
            && p["entryPointVersion"] == "0.7"
            && addr(&p["entryPoint"])? == ENTRY_POINT
            && addr(&p["delegation"])? == DELEGATION,
        "unexpected chain, EntryPoint or delegation",
    )?;
    let op = &p["userOperation"];
    fields(
        op,
        &[
            "sender",
            "nonce",
            "callData",
            "callGasLimit",
            "verificationGasLimit",
            "preVerificationGas",
            "maxFeePerGas",
            "maxPriorityFeePerGas",
            "paymaster",
            "paymasterData",
            "paymasterVerificationGasLimit",
            "paymasterPostOpGasLimit",
        ],
    )?;
    ensure(
        addr(&op["sender"])? == owner && bytes(&op["callData"])?.as_ref() == expected.call_data,
        "operation does not match payment",
    )?;
    let nonce = quantity(&op["nonce"], 256)?;
    ensure(
        nonce >> 64 == U256::from(1),
        "unsupported account nonce key",
    )?;
    let call_gas = quantity(&op["callGasLimit"], 128)?;
    let verify_gas = quantity(&op["verificationGasLimit"], 128)?;
    ensure(
        call_gas > U256::ZERO
            && verify_gas > U256::ZERO
            && addr(&op["paymaster"])? == Address::ZERO
            && op["paymasterData"] == "0x"
            && [
                "preVerificationGas",
                "maxFeePerGas",
                "maxPriorityFeePerGas",
                "paymasterVerificationGasLimit",
                "paymasterPostOpGasLimit",
            ]
            .iter()
            .all(|field| op[field] == "0x0"),
        "unexpected bundler sponsorship profile",
    )?;
    // EntryPoint 0.7 uses empty initCode and paymasterAndData for this pinned sponsorship profile.
    let mut packed = [0u8; 256];
    packed[12..32].copy_from_slice(owner.as_slice());
    packed[32..64].copy_from_slice(&nonce.to_be_bytes::<32>());
    packed[64..96].copy_from_slice(keccak256([]).as_slice());
    packed[96..128].copy_from_slice(keccak256(&expected.call_data).as_slice());
    packed[128..144].copy_from_slice(&verify_gas.to_be_bytes::<32>()[16..]);
    packed[144..160].copy_from_slice(&call_gas.to_be_bytes::<32>()[16..]);
    packed[224..256].copy_from_slice(keccak256([]).as_slice());
    let mut outer = [0u8; 96];
    outer[..32].copy_from_slice(keccak256(packed).as_slice());
    outer[44..64].copy_from_slice(ENTRY_POINT.as_slice());
    outer[64..96].copy_from_slice(&U256::from(expected.chain).to_be_bytes::<32>());
    let hash = keccak256(outer);
    ensure(
        bytes(&p["userOperationHash"])?.as_ref() == hash.as_slice(),
        "operation hash mismatch",
    )?;
    let authorization = p
        .get("authorization")
        .map(|a| {
            fields(a, &["address", "chainId", "nonce"])?;
            ensure(
                addr(&a["address"])? == DELEGATION && integer(&a["chainId"])? == expected.chain,
                "unexpected delegation authorization",
            )?;
            Ok(Authorization {
                chain_id: U256::from(expected.chain),
                address: DELEGATION,
                nonce: integer(&a["nonce"])?,
            })
        })
        .transpose()?;
    Ok((hash, authorization))
}

fn check_expiry(p: &Value, deadline: U256) -> Result<(), Error> {
    let expires = integer(&p["expiresAt"])?;
    ensure(
        expires > now() && U256::from(expires) <= deadline,
        "sponsorship expired or exceeds payment deadline",
    )
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn invalid(message: &str) -> Error {
    Error::new("X402_EIP7702_INVALID", message)
}
fn ensure(value: bool, message: &str) -> Result<(), Error> {
    if value { Ok(()) } else { Err(invalid(message)) }
}
fn fields(v: &Value, allowed: &[&str]) -> Result<(), Error> {
    ensure(
        v.as_object()
            .is_some_and(|o| o.keys().all(|k| allowed.contains(&k.as_str()))),
        "unsupported sponsorship fields",
    )
}
fn addr(v: &Value) -> Result<Address, Error> {
    v.as_str()
        .filter(|s| s.starts_with("0x") && s.len() == 42)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| invalid("sponsorship address"))
}
fn bytes(v: &Value) -> Result<Bytes, Error> {
    v.as_str()
        .filter(|s| s.starts_with("0x") && s.len() % 2 == 0)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| invalid("sponsorship bytes"))
}
fn decimal(v: &Value) -> Result<U256, Error> {
    let s = v
        .as_str()
        .filter(|s| {
            !s.is_empty()
                && (s.len() == 1 || !s.starts_with('0'))
                && s.bytes().all(|b| b.is_ascii_digit())
        })
        .ok_or_else(|| invalid("payment amount or nonce"))?;
    U256::from_str_radix(s, 10).map_err(|_| invalid("payment amount or nonce"))
}
fn quantity(v: &Value, bits: usize) -> Result<U256, Error> {
    let s = v
        .as_str()
        .and_then(|s| s.strip_prefix("0x"))
        .filter(|s| {
            !s.is_empty()
                && (s.len() == 1 || !s.starts_with('0'))
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .ok_or_else(|| invalid("sponsorship quantity"))?;
    let n = U256::from_str_radix(s, 16).map_err(|_| invalid("sponsorship quantity"))?;
    ensure(n.bit_len() <= bits, "sponsorship quantity overflow")?;
    Ok(n)
}
fn integer(v: &Value) -> Result<u64, Error> {
    v.as_u64()
        .filter(|v| *v <= 9_007_199_254_740_991)
        .ok_or_else(|| invalid("sponsorship integer"))
}
