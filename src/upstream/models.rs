//! `GET /models` pull and catalog rebuild (spec 10 §2.4, spec 15 §2.2, §5).

use serde_json::Value;

use super::client::{classify_attempt, Endpoint, UpstreamClient, UpstreamRequest};
use super::token::mint_token;
use crate::catalog::Catalog;
use crate::error::GatewayError;

/// Fetch the upstream catalog for one account: mint a token, then read
/// `/models`. Every non-2xx and every unusable body is a failure; the caller
/// keeps the previous registry (spec 15 §2.2, §6.3).
pub async fn fetch_models(
    client: &dyn UpstreamClient,
    user_id: &str,
    credential: &Value,
) -> Result<Catalog, GatewayError> {
    let minted = mint_token(client, user_id, credential).await?;
    let request = UpstreamRequest::new(Endpoint::Models)
        .bearer(&minted.token)
        .zed_version();
    let response = client.execute(request).await?;
    if let Some(failure) = classify_attempt(response.status, &response.body, false) {
        return Err(failure.error);
    }
    let text = String::from_utf8_lossy(&response.body);
    let trimmed = text.trim_start();
    if !trimmed.starts_with('{') {
        return Err(GatewayError::transient(
            Some(response.status),
            "models response is not a JSON object",
        ));
    }
    let value: Value = serde_json::from_str(trimmed).map_err(|_| {
        GatewayError::transient(Some(response.status), "models response is not valid JSON")
    })?;
    Catalog::from_zed_models(&value).ok_or_else(|| {
        GatewayError::transient(Some(response.status), "models response has no models array")
    })
}
