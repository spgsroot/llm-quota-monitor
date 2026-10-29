use crate::auth_types::AuthError;
use reqwest::{Response, StatusCode};
use serde::de::DeserializeOwned;
use zeroize::Zeroizing;

pub async fn json<T: DeserializeOwned>(
    mut response: Response,
    limit: usize,
) -> Result<(StatusCode, T), AuthError> {
    let status = response.status();
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(AuthError::new("Provider response is too large"));
    }
    let mut body = Zeroizing::new(Vec::with_capacity(
        response.content_length().unwrap_or(4096).min(limit as u64) as usize,
    ));
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| AuthError::new("Cannot read provider response"))?
    {
        if body.len() + chunk.len() > limit {
            return Err(AuthError::new("Provider response is too large"));
        }
        body.extend_from_slice(&chunk);
    }
    let value = serde_json::from_slice(&body)
        .map_err(|_| AuthError::new("Provider returned invalid JSON"))?;
    Ok((status, value))
}
