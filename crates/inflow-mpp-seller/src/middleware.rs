use crate::{CancellationToken, Error, Offer, decode_credential, render_challenge};
use mpp::{MppError, server::middleware::PaymentVerifier};
use std::{future::Future, pin::Pin};

impl PaymentVerifier for Offer {
    fn challenge(&self) -> Result<String, String> {
        self.challenge(None)
            .and_then(|value| render_challenge(&value))
            .map_err(|error| error.to_string())
    }

    fn challenge_with_body(&self, body: &[u8]) -> Result<String, String> {
        self.challenge(Some(body))
            .and_then(|value| render_challenge(&value))
            .map_err(|error| error.to_string())
    }

    fn verify(
        &self,
        credential: &str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> {
        let future = self.verify_credential(credential, None);
        Box::pin(async move { future.await.map_err(|error| error.to_string()) })
    }

    fn verify_with_body(
        &self,
        credential: &str,
        body: &[u8],
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> {
        let future = self.verify_credential(credential, Some(body));
        Box::pin(async move { future.await.map_err(|error| error.to_string()) })
    }

    fn verify_credential(
        &self,
        credential: &str,
        body: Option<&[u8]>,
    ) -> Pin<Box<dyn Future<Output = Result<String, MppError>> + Send>> {
        let offer = self.clone();
        let credential = credential.to_owned();
        let body = body.map(<[u8]>::to_vec);
        Box::pin(async move {
            let encoded = credential
                .split_once(char::is_whitespace)
                .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Payment"))
                .map(|(_, encoded)| encoded.trim())
                .ok_or(MppError::MalformedCredential(None))?;
            let credential =
                decode_credential(encoded).map_err(|_| MppError::MalformedCredential(None))?;
            let receipt = offer
                .accept(&credential, body.as_deref(), &CancellationToken::new())
                .await
                .map_err(payment_error)?;
            receipt.to_header()
        })
    }

    fn credential_header(&self) -> &str {
        self.credential_header()
    }
}

fn payment_error(error: Error) -> MppError {
    match error.code.as_str() {
        "INVALID_MPP_DATA" => MppError::MalformedCredential(None),
        "MPP_CREDENTIAL_MISMATCH" => MppError::InvalidChallenge {
            id: None,
            reason: None,
        },
        // Only an explicit payment rejection invites another payment attempt.
        // Transport failures and malformed settlement responses can hide a completed charge.
        "MPP_PAYMENT_FAILED" if error.body["status"] == 402 => {
            MppError::verification_failed_default()
        }
        "MPP_PAYMENT_FAILED" if error.body["status"] == 403 => MppError::CredentialReplayed(None),
        _ => MppError::Internal(error.to_string()),
    }
}
