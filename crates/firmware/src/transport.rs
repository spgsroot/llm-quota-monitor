use esp_idf_svc::http::{
    client::{Configuration, EspHttpConnection, FollowRedirectsPolicy},
    Method,
};
use quota_common::{QuotaSnapshot, MAX_RESPONSE_BYTES};
use std::time::{Duration, Instant};

pub fn fetch(url: &str, body: &mut Vec<u8>) -> Result<QuotaSnapshot, &'static str> {
    let started = Instant::now();
    let mut connection = EspHttpConnection::new(&Configuration {
        timeout: Some(Duration::from_secs(3)),
        follow_redirects_policy: FollowRedirectsPolicy::FollowNone,
        buffer_size: Some(1024),
        ..Default::default()
    })
    .map_err(|_| "HTTP client initialization failed")?;
    connection
        .initiate_request(Method::Get, url, &[("Accept", "application/json")])
        .map_err(|_| "HTTP connect/request failed")?;
    connection
        .initiate_response()
        .map_err(|_| "HTTP response failed")?;
    if connection.status() != 200 {
        return Err("Collector returned non-200 status");
    }
    let expected = connection
        .header("Content-Length")
        .and_then(|value| value.parse::<usize>().ok());
    if expected.is_some_and(|size| size > MAX_RESPONSE_BYTES) {
        return Err("Collector response too large");
    }
    body.clear();
    let mut chunk = [0_u8; 512];
    loop {
        if started.elapsed() >= Duration::from_secs(10) {
            return Err("HTTP response deadline exceeded");
        }
        let count = connection
            .read(&mut chunk)
            .map_err(|_| "HTTP body read failed")?;
        if count == 0 {
            break;
        }
        if body.len() + count > MAX_RESPONSE_BYTES {
            return Err("Collector response too large");
        }
        body.extend_from_slice(&chunk[..count]);
    }
    if expected.is_some_and(|length| length != body.len()) {
        return Err("Incomplete collector response");
    }
    let snapshot: QuotaSnapshot = serde_json::from_slice(body).map_err(|_| "Invalid quota JSON")?;
    snapshot.validate()?;
    Ok(snapshot)
}
