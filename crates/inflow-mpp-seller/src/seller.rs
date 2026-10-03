use crate::{CancellationToken, ClientOptions, Credential, Error, PaymentChallenge, Receipt};
use inflow_mpp::{
    Base64UrlJson, decode, decode_receipt, encode, internal::MppClient, validate_request,
};
use mpp::{
    ChargeMethod, ChargeRequest, ChargeValidation, VerificationError,
    protocol::core::PaymentCredential, server::Mpp,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Inflow,
    Tempo,
}
impl Method {
    fn name(self) -> &'static str {
        match self {
            Self::Inflow => "inflow",
            Self::Tempo => "tempo",
        }
    }
}

pub struct SellerOptions {
    pub realm: String,
    /// Local challenge-signing secret, separate from the InFlow API key.
    pub secret_key: String,
}

#[derive(Clone, Default)]
pub struct ChallengeOptions {
    /// RFC 3339 expiration. Defaults to five minutes after challenge creation.
    pub expires: Option<String>,
    pub description: Option<String>,
    pub opaque: Option<Base64UrlJson>,
    /// Use Payment-Authorization, leaving Authorization available for Service authentication.
    pub requires_auth: bool,
}

struct Inner {
    client: MppClient,
    config: Value,
    options: SellerOptions,
}

#[derive(Clone)]
pub struct Seller(Arc<Inner>);

impl Seller {
    /// Loads authenticated configuration before returning. Retry construction after a failed load.
    pub async fn new(
        client: ClientOptions,
        options: SellerOptions,
        cancellation: &CancellationToken,
    ) -> Result<Self, Error> {
        if options.realm.trim().is_empty() {
            return Err(invalid("realm must be nonempty"));
        }
        // Match mppx's construction boundary; mpp::Mpp::new does not validate key length.
        if options.secret_key.len() < 32 {
            return Err(invalid("signing secret must be at least 32 bytes"));
        }
        let client = MppClient::new(client)?;
        let config = client.config(cancellation).await?;
        if config["sellerId"].as_str().is_none_or(str::is_empty)
            || !config["supportedMethods"].is_array()
        {
            return Err(invalid("invalid Seller configuration"));
        }
        Ok(Self(Arc::new(Inner {
            client,
            config,
            options,
        })))
    }

    /// Creates immutable route payment terms. Amounts are decimal strings for InFlow and base-unit strings for Tempo.
    pub fn offer(
        &self,
        method: Method,
        mut request: Value,
        options: ChallengeOptions,
    ) -> Result<Offer, Error> {
        if !request.is_object() {
            return Err(invalid("request must be an object"));
        }
        match method {
            Method::Inflow => {
                validate_request(method.name(), "charge", &request)?;
                request["recipient"] = self.0.config["sellerId"].clone();
                request["methodDetails"] = rails(&self.0.config, &request)?;
            }
            Method::Tempo => {
                if !request.get("methodDetails").is_some_and(Value::is_object) {
                    if request.get("methodDetails").is_some() {
                        return Err(invalid("methodDetails must be an object"));
                    }
                    request["methodDetails"] = json!({});
                }
                let details = &mut request["methodDetails"];
                if details.get("feePayer").is_none() {
                    details["feePayer"] = json!(false);
                }
                if details.get("supportedModes").is_none() {
                    details["supportedModes"] = json!(["pull"]);
                }
                if request.get("currency").is_none() || request.get("recipient").is_none() {
                    return Err(invalid("Tempo currency and recipient are required"));
                }
            }
        }
        validate_request(method.name(), "charge", &request)?;
        let expected = serde_json::from_value(request.clone()).map_err(json_error)?;
        if let Some(expires) = &options.expires {
            OffsetDateTime::parse(expires, &Rfc3339)
                .map_err(|_| invalid("invalid challenge expiration"))?;
        }
        Ok(Offer {
            seller: self.clone(),
            method,
            request,
            expected,
            options,
        })
    }
}

#[derive(Clone)]
pub struct Offer {
    seller: Seller,
    method: Method,
    request: Value,
    expected: ChargeRequest,
    options: ChallengeOptions,
}

/// Non-mutating acceptance result. It is not a receipt and does not authorize resource delivery.
#[derive(Clone, Serialize)]
pub struct Validation {
    pub success: bool,
    pub challenge: PaymentChallenge,
    pub credential: Credential,
    pub details: Value,
    pub method: String,
    pub intent: String,
    pub request: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl Offer {
    pub fn request(&self) -> &Value {
        &self.request
    }

    /// Pass the actual request body when payment is bound to body bytes; use the same bytes at acceptance.
    pub fn challenge(&self, body: Option<&[u8]>) -> Result<PaymentChallenge, Error> {
        let expires = match &self.options.expires {
            Some(value) => value.clone(),
            None => (OffsetDateTime::now_utc() + Duration::minutes(5))
                .format(&Rfc3339)
                .map_err(|_| invalid("challenge expiration"))?,
        };
        let digest = body.map(mpp::body_digest::compute);
        let challenge = PaymentChallenge::with_secret_key_full(
            &self.seller.0.options.secret_key,
            &self.seller.0.options.realm,
            self.method.name(),
            "charge",
            Base64UrlJson::from_raw(encode(&self.request)?),
            Some(&expires),
            digest.as_deref(),
            self.options.description.as_deref(),
            self.options.opaque.clone(),
            self.options
                .requires_auth
                .then_some("Payment-Authorization"),
        );
        inflow_mpp::render_challenge(&challenge)?;
        Ok(challenge)
    }

    pub async fn validate(
        &self,
        credential: &Credential,
        body: Option<&[u8]>,
        cancellation: &CancellationToken,
    ) -> Result<Validation, Error> {
        let (bridge, projected, expected) = self.bridge(credential, cancellation)?;
        let server = self.server(bridge.clone());
        let result = match body {
            Some(body) => {
                server
                    .validate_credential_with_expected_request_and_body(&projected, &expected, body)
                    .await
            }
            None => {
                server
                    .validate_credential_with_expected_request(&projected, &expected)
                    .await
            }
        }
        .map_err(|e| bridge.error(e))?;
        Ok(Validation {
            success: true,
            challenge: credential.challenge.clone(),
            credential: credential.clone(),
            details: result.details,
            method: self.method.name().into(),
            intent: "charge".into(),
            request: decode(credential.challenge.request.raw())?,
            source: credential.source.clone(),
        })
    }

    /// Re-validates and settles. Deliver the resource only after this returns a receipt.
    pub async fn accept(
        &self,
        credential: &Credential,
        body: Option<&[u8]>,
        cancellation: &CancellationToken,
    ) -> Result<Receipt, Error> {
        let (bridge, projected, expected) = self.bridge(credential, cancellation)?;
        let server = self.server(bridge.clone());
        match body {
            Some(body) => {
                server
                    .broadcast_credential_with_expected_request_and_body(
                        &projected, &expected, body,
                    )
                    .await
            }
            None => {
                server
                    .broadcast_credential_with_expected_request(&projected, &expected)
                    .await
            }
        }
        .map_err(|e| bridge.error(e))
    }

    fn bridge(
        &self,
        credential: &Credential,
        cancellation: &CancellationToken,
    ) -> Result<(Bridge, PaymentCredential, ChargeRequest), Error> {
        inflow_mpp::encode_credential(credential)?;
        let request = decode(credential.challenge.request.raw())?;
        validate_request(self.method.name(), "charge", &request)?;
        if binding(self.method, &request) != binding(self.method, &self.request) {
            return Err(Error::new(
                "MPP_CREDENTIAL_MISMATCH",
                "credential does not match this offer",
            ));
        }
        let wire = serde_json::to_value(credential).map_err(json_error)?;
        // mpp0.14 ChallengeEcho omits description (upstream PR490). Preserve the original per request;
        // only the verification projection enters upstream, never the platform-bound credential.
        let projected = PaymentCredential {
            challenge: credential.challenge.to_echo(),
            payload: Value::Object(credential.payload.clone()),
            source: credential.source.clone(),
        };
        let expected = self.expected.clone();
        let bridge = Bridge {
            seller: self.seller.clone(),
            method: self.method,
            wire,
            cancellation: cancellation.clone(),
            error: Arc::new(Mutex::new(None)),
        };
        Ok((bridge, projected, expected))
    }

    fn server(&self, bridge: Bridge) -> Mpp<Bridge> {
        let mut server = Mpp::new(
            bridge,
            &self.seller.0.options.realm,
            &self.seller.0.options.secret_key,
        )
        .with_requires_auth(self.options.requires_auth);
        if let Some(opaque) = &self.options.opaque {
            server = server.with_opaque(opaque.clone());
        }
        server
    }
}

#[derive(Clone)]
struct Bridge {
    seller: Seller,
    method: Method,
    wire: Value,
    cancellation: CancellationToken,
    error: Arc<Mutex<Option<Error>>>,
}
impl Bridge {
    fn retain(&self, error: Error) -> VerificationError {
        *self
            .error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
        VerificationError::new("platform rejected payment")
    }
    fn error(&self, error: VerificationError) -> Error {
        self.error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or_else(|| Error::new("MPP_CREDENTIAL_MISMATCH", error.to_string()))
    }
}
impl ChargeMethod for Bridge {
    fn method(&self) -> &str {
        self.method.name()
    }
    fn supports_validation(&self) -> bool {
        true
    }
    async fn validate(
        &self,
        credential: &PaymentCredential,
        request: &ChargeRequest,
    ) -> Result<ChargeValidation, VerificationError> {
        let result = self
            .seller
            .0
            .client
            .validate(self.wire.clone(), &self.cancellation)
            .await
            .map_err(|e| self.retain(e))?;
        if result["success"] != true {
            return Err(self.retain(problem(&result, "validation")));
        }
        let challenge = &self.wire["challenge"];
        if !result["request"].is_object()
            || result.get("details").is_some_and(|v| !v.is_object())
            || result.get("source") != self.wire.get("source")
            || result["method"] != challenge["method"]
            || result["intent"] != challenge["intent"]
            || encode(&result["challenge"]).ok() != encode(challenge).ok()
            || encode(&result["credential"]).ok() != encode(&self.wire).ok()
        {
            return Err(self.retain(problem(&json!({}), "validation")));
        }
        Ok(ChargeValidation::new(
            credential,
            request,
            result.get("details").cloned().unwrap_or_else(|| json!({})),
        ))
    }
    async fn broadcast(
        &self,
        _: &PaymentCredential,
        _: &ChargeRequest,
    ) -> Result<Receipt, VerificationError> {
        let client = &self.seller.0.client;
        let result = if self.seller.0.config["featureFlags"]["idempotencyKeyEnabled"] == true {
            // One key per terminal call; the HTTP runtime retains it across transport retries.
            client
                .broadcast(
                    self.wire.clone(),
                    &uuid::Uuid::new_v4().to_string(),
                    &self.cancellation,
                )
                .await
        } else {
            client
                .broadcast_without_key(self.wire.clone(), &self.cancellation)
                .await
        }
        .map_err(|e| self.retain(e))?;
        encode(&result["receipt"])
            .and_then(|v| decode_receipt(&v))
            .map_err(|_| self.retain(problem(&result, "broadcast")))
    }
    async fn verify(
        &self,
        credential: &PaymentCredential,
        request: &ChargeRequest,
    ) -> Result<Receipt, VerificationError> {
        self.validate(credential, request).await?;
        self.broadcast(credential, request).await
    }
}

fn rails(config: &Value, request: &Value) -> Result<Value, Error> {
    let currency = request["currency"].as_str().unwrap_or_default();
    let method = config["supportedMethods"]
        .as_array()
        .and_then(|v| v.iter().find(|v| v["id"] == "inflow"))
        .cloned()
        .unwrap_or(Value::Null);
    let details = &method["methodDetails"];
    let advertised = if details["intentCurrencyRails"]
        .as_object()
        .is_some_and(|v| !v.is_empty())
    {
        details["intentCurrencyRails"]["charge"][currency]
            .as_array()
            .cloned()
            .unwrap_or_default()
    } else {
        details["currencyRails"]
            .get(currency)
            .cloned()
            .into_iter()
            .collect()
    };
    let requested = request["methodDetails"].get("rail");
    if advertised.is_empty() {
        return Err(Error::new(
            "MPP_UNSUPPORTED_CURRENCY",
            "currency is not supported for charge",
        ));
    }
    if requested.is_none() && advertised.len() > 1 {
        return Err(Error::new(
            "MPP_AMBIGUOUS_RAIL",
            "select a rail for this currency",
        ));
    }
    let selected = advertised
        .iter()
        .find(|v| requested.is_none_or(|rail| &v["rail"] == rail))
        .filter(|v| matches!(v["rail"].as_str(), Some("balance" | "instrument")))
        .ok_or_else(|| {
            Error::new(
                "MPP_UNSUPPORTED_RAIL",
                "rail is not supported for this currency",
            )
        })?;
    let instrument = request["methodDetails"].get("instrumentId");
    if selected["rail"] == "instrument"
        && selected["instrumentId"] == "required"
        && instrument.is_none()
    {
        return Err(Error::new(
            "MPP_INSTRUMENT_REQUIRED",
            "an instrument identifier is required",
        ));
    }
    let mut result = json!({"rail":selected["rail"]});
    if let Some(instrument) = instrument {
        result["instrumentId"] = instrument.clone();
    }
    Ok(result)
}

fn binding(method: Method, request: &Value) -> Value {
    // Upstream expected-request checks omit method-specific fields (issue555).
    // Match Node's stableBinding so valid credentials cannot change rail or Tempo transfer terms.
    match method {
        Method::Inflow => {
            json!({
                "amount": request["amount"],
                "currency": request["currency"],
                "recipient": request["recipient"],
                "rail": request["methodDetails"].get("rail").cloned().unwrap_or(json!("balance")),
                "instrumentId": request["methodDetails"]["instrumentId"]
            })
        }
        Method::Tempo => {
            json!({
                "amount": request["amount"],
                "currency": request["currency"],
                "recipient": request["recipient"],
                "description": request["description"],
                "externalId": request["externalId"],
                "chainId": request["methodDetails"]["chainId"],
                "feePayer": request["methodDetails"]["feePayer"],
                "memo": request["methodDetails"]["memo"],
                "splits": request["methodDetails"]["splits"],
                "supportedModes": request["methodDetails"]["supportedModes"]
            })
        }
    }
}
fn invalid(message: &str) -> Error {
    Error::new("INVALID_MPP_DATA", message)
}
fn json_error(_: serde_json::Error) -> Error {
    invalid("invalid payment data")
}
fn problem(result: &Value, operation: &str) -> Error {
    let body = result.get("problem").cloned().unwrap_or_else(|| {
        json!({
            "type": "https://paymentauth.org/problems/verification-failed",
            "title": "Verification Failed",
            "status": 402,
            "detail": format!("The PSP {operation} response was malformed.")
        })
    });
    let message = body["detail"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or("payment verification failed");
    let mut error = Error::new("MPP_PAYMENT_FAILED", message);
    *error.body = body;
    error
}

#[cfg(test)]
#[path = "../tests/support/seller.rs"]
mod tests;
