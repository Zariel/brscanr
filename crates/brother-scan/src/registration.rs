use std::{net::SocketAddrV4, time::Duration};

use digest_auth::AuthContext;
use reqwest::{Client, StatusCode, header};
use serde_json::json;

use crate::{Result, protocol};

/// Phoenix registration. The printer's public protocol identity is Public:0000.
pub struct Registration {
    client: Client,
    url: String,
    name: String,
    callback: SocketAddrV4,
}

impl Registration {
    /// `insecure_tls` explicitly accepts the printer's self-signed certificate.
    pub fn new(
        printer: std::net::Ipv4Addr,
        callback: SocketAddrV4,
        name: String,
        insecure_tls: bool,
    ) -> Result<Self> {
        if name.is_empty()
            || name.len() > 32
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b' ' || b == b'-' || b == b'_')
        {
            return Err(protocol(
                "destination name must be 1–32 ASCII letters, digits, spaces, '-' or '_'",
            ));
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .danger_accept_invalid_certs(insecure_tls)
            .build()?;
        Ok(Self {
            client,
            url: format!("https://{printer}/phoenix/mib"),
            name,
            callback,
        })
    }

    /// Request a six-minute lease for each standard Scan to PC function.
    pub async fn renew(&self) -> Result<()> {
        let request: Vec<_> = [("IMAGE", 1), ("OCR", 3), ("EMAIL", 2), ("FILE", 5)]
            .into_iter().map(|(function, number)| json!({
                "key": "1.3.6.1.4.1.2435.2.3.9.2.11.1.1.0",
                "string_value": format!("TYPE=BR;BUTTON=SCAN;USER={};FUNC={function};HOST={};APPNUM={number};DURATION=360;BRID=;", self.name, self.callback)
            })).collect();
        let body = json!({"request": request}).to_string();
        let send = || {
            self.client
                .post(&self.url)
                .header(header::CONTENT_TYPE, "application/json")
                .body(body.clone())
        };
        let response = send().send().await?;
        let response = if response.status() == StatusCode::UNAUTHORIZED {
            let challenge = response
                .headers()
                .get(header::WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| protocol("missing Digest challenge"))?
                .to_owned();
            bounded_body(response).await?;
            let mut prompt =
                digest_auth::parse(&challenge).map_err(|_| protocol("invalid Digest challenge"))?;
            let context =
                AuthContext::new_post("Public", "0000", "/phoenix/mib", Some(body.as_bytes()));
            let answer = prompt
                .respond(&context)
                .map_err(|_| protocol("unsupported Digest challenge"))?
                .to_string();
            send().header(header::AUTHORIZATION, answer).send().await?
        } else {
            response
        };
        let response = response.error_for_status()?;
        let result: serde_json::Value = serde_json::from_slice(&bounded_body(response).await?)
            .map_err(|_| protocol("invalid registration JSON"))?;
        let entries = result["response"]
            .as_array()
            .ok_or_else(|| protocol("missing registration results"))?;
        if entries.len() != 4 || entries.iter().any(|v| v["status"].as_i64() != Some(0)) {
            return Err(protocol("printer rejected registration"));
        }
        Ok(())
    }
}

async fn bounded_body(mut response: reqwest::Response) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if data.len() + chunk.len() > 65536 {
            return Err(protocol("registration response too large"));
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, http::HeaderMap, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn digest_renewals() {
        let count = Arc::new(AtomicUsize::new(0));
        let received = Arc::clone(&count);
        let router = Router::new().route("/phoenix/mib", post(move |headers: HeaderMap, Json(body): Json<serde_json::Value>| {
            let received = Arc::clone(&received);
            async move {
                use axum::response::IntoResponse;
                if !headers.contains_key(header::AUTHORIZATION) {
                    return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Digest realm=\"printer\", nonce=\"abc123\", qop=\"auth\", algorithm=MD5")]).into_response();
                }
                let auth = headers[header::AUTHORIZATION].to_str().unwrap();
                assert!(auth.starts_with("Digest username=\"Public\""));
                assert!(auth.contains("uri=\"/phoenix/mib\""));
                let entries = body["request"].as_array().unwrap();
                assert_eq!(entries.len(), 4);
                for (entry, function) in entries.iter().zip(["IMAGE", "OCR", "EMAIL", "FILE"]) {
                    assert_eq!(entry["key"], "1.3.6.1.4.1.2435.2.3.9.2.11.1.1.0");
                    let value = entry["string_value"].as_str().unwrap();
                    assert!(value.contains(&format!("USER=Office;FUNC={function};HOST=192.0.2.1:54925;")));
                    assert!(value.ends_with("DURATION=360;BRID=;"));
                }
                received.fetch_add(1, Ordering::Relaxed);
                Json(json!({"response": [{"status":0},{"status":0},{"status":0},{"status":0}]})).into_response()
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/phoenix/mib", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let mut registration = Registration::new(
            "192.0.2.3".parse().unwrap(),
            "192.0.2.1:54925".parse().unwrap(),
            "Office".into(),
            false,
        )
        .unwrap();
        registration.url = url;
        for _ in 0..20 {
            registration.renew().await.unwrap();
        }
        assert_eq!(count.load(Ordering::Relaxed), 20);
        server.abort();
    }
}
