use std::{path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use reqwest::{Client, StatusCode, header, multipart};
use serde::Deserialize;

pub struct Paperless {
    client: Client,
    base: url::Url,
}

pub enum Upload {
    Submitted(String),
    Retry,
    Rejected(String),
    Unknown(String),
}

pub enum Outcome {
    Pending,
    Imported(i64),
    Failed,
    Missing,
}

#[derive(Deserialize)]
struct Task {
    task_id: String,
    status: String,
    related_document: Option<serde_json::Value>,
}

impl Paperless {
    pub fn new(base: url::Url, token: &str) -> Result<Self> {
        let mut headers = header::HeaderMap::new();
        let mut auth = header::HeaderValue::from_str(&format!("Token {token}"))?;
        auth.set_sensitive(true);
        headers.insert(header::AUTHORIZATION, auth);
        // API v9 preserves the task list and Celery status contract across upgrades.
        headers.insert(
            header::ACCEPT,
            header::HeaderValue::from_static("application/json; version=9"),
        );
        let client = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { client, base })
    }

    pub async fn upload(&self, id: &str, pdf: &Path) -> Result<Upload> {
        // Prepare the file before crossing the network ambiguity boundary.
        let file = tokio::fs::File::open(pdf).await?;
        let size = file.metadata().await?.len();
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
        let part = multipart::Part::stream_with_length(body, size)
            .file_name(format!("brscanr-{id}.pdf"))
            .mime_str("application/pdf")?;
        let response = self
            .client
            .post(self.base.join("api/documents/post_document/")?)
            .multipart(multipart::Form::new().part("document", part))
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(e) if e.is_connect() => return Ok(Upload::Retry),
            Err(_) => {
                return Ok(Upload::Unknown(
                    "Upload connection failed after it may have reached Paperless".into(),
                ));
            }
        };
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Ok(Upload::Retry);
        }
        if [400, 401, 403, 404, 413, 415, 422].contains(&status.as_u16()) {
            return Ok(Upload::Rejected(format!(
                "Paperless rejected upload: HTTP {status}"
            )));
        }
        if !status.is_success() {
            return Ok(Upload::Unknown(format!(
                "Upload outcome requires reconciliation: HTTP {status}"
            )));
        }
        let bytes = match bounded_body(response).await {
            Ok(bytes) => bytes,
            Err(_) => {
                return Ok(Upload::Unknown(
                    "Upload accepted but task response could not be read".into(),
                ));
            }
        };
        let id = serde_json::from_slice::<String>(&bytes)
            .ok()
            .and_then(|s| uuid::Uuid::parse_str(&s).ok());
        match id {
            Some(id) => Ok(Upload::Submitted(id.to_string())),
            None => Ok(Upload::Unknown(
                "Upload accepted but no valid task UUID was returned".into(),
            )),
        }
    }

    pub async fn poll(&self, id: &str) -> Result<Outcome> {
        let response = self
            .client
            .get(self.base.join("api/tasks/")?)
            .query(&[("task_id", id)])
            .send()
            .await?
            .error_for_status()?;
        let tasks: Vec<Task> = serde_json::from_slice(&bounded_body(response).await?)
            .context("invalid task response")?;
        let Some(task) = tasks.iter().find(|t| t.task_id == id) else {
            return Ok(Outcome::Missing);
        };
        match task.status.as_str() {
            "SUCCESS" => {
                let value = task
                    .related_document
                    .as_ref()
                    .context("successful task has no document ID")?;
                let id: i64 = match value {
                    serde_json::Value::String(s) => s.parse()?,
                    _ => value.as_i64().context("invalid document ID")?,
                };
                if id <= 0 {
                    bail!("invalid Paperless document ID");
                }
                Ok(Outcome::Imported(id))
            }
            "FAILURE" | "REVOKED" => Ok(Outcome::Failed),
            "PENDING" | "STARTED" | "RETRY" => Ok(Outcome::Pending),
            _ => bail!("unrecognized Paperless task status"),
        }
    }
}

async fn bounded_body(mut response: reqwest::Response) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > 1024 * 1024 {
            bail!("Paperless response exceeds 1 MiB");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
