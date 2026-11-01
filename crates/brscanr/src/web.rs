use std::sync::Arc;

use axum::{
    Form, Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use brother_scan::Source;
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    service::App,
    store::{Admission, Job, now},
};

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/scan", post(scan_form))
        .route("/scanner/acknowledge", post(acknowledge))
        .route("/jobs/{id}", get(job_page))
        .route("/jobs/{id}/document.pdf", get(download))
        .route("/jobs/{id}/retry", post(retry))
        .route("/jobs/{id}/resolve", post(resolve))
        .route("/api/scans", post(scan_api))
        .route("/api/jobs", get(jobs))
        .route("/api/jobs/{id}", get(job_api))
        .route("/api/status", get(status))
        .route("/livez", get(|| async { "ok" }))
        .route("/readyz", get(ready))
        .route("/metrics", get(metrics))
        .layer(DefaultBodyLimit::max(4096))
        .with_state(app)
}

type WebResult<T> = Result<T, WebError>;
pub struct WebError(StatusCode, String);

impl From<anyhow::Error> for WebError {
    fn from(error: anyhow::Error) -> Self {
        tracing::error!(%error, "HTTP request failed");
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Operation failed; inspect service logs".into(),
        )
    }
}
impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

#[derive(Deserialize)]
struct ScanRequest {
    source: Source,
    idempotency_key: Uuid,
}
#[derive(Deserialize)]
struct ScanForm {
    source: Source,
    idempotency_key: Uuid,
    csrf: String,
}
#[derive(Deserialize)]
struct Action {
    csrf: String,
    #[serde(default)]
    confirm: Option<String>,
    document_id: Option<i64>,
}

fn check_csrf(app: &App, token: &str) -> WebResult<()> {
    if token != app.csrf {
        return Err(WebError(
            StatusCode::FORBIDDEN,
            "Form expired; reload the page".into(),
        ));
    }
    Ok(())
}

async fn scan_form(State(app): State<Arc<App>>, Form(form): Form<ScanForm>) -> WebResult<Redirect> {
    check_csrf(&app, &form.csrf)?;
    let result = app
        .submit(&format!("ui:{}", form.idempotency_key), form.source, "web")
        .await?;
    match result {
        Admission::Started(job) | Admission::Existing(job) => {
            Ok(Redirect::to(&format!("/jobs/{}", job.id)))
        }
        Admission::Rejected(reason) => Err(WebError(StatusCode::CONFLICT, reason)),
    }
}
async fn scan_api(
    State(app): State<Arc<App>>,
    Json(form): Json<ScanRequest>,
) -> WebResult<Response> {
    match app
        .submit(&format!("ui:{}", form.idempotency_key), form.source, "api")
        .await?
    {
        Admission::Started(job) => Ok((StatusCode::ACCEPTED, Json(job)).into_response()),
        Admission::Existing(job) => Ok((StatusCode::OK, Json(job)).into_response()),
        Admission::Rejected(reason) => Err(WebError(StatusCode::CONFLICT, reason)),
    }
}
async fn acknowledge(State(app): State<Arc<App>>, Form(form): Form<Action>) -> WebResult<Redirect> {
    check_csrf(&app, &form.csrf)?;
    let _guard = app.admission.lock().await;
    app.store.acknowledge().await?;
    Ok(Redirect::to("/"))
}
async fn retry(
    State(app): State<Arc<App>>,
    Path(id): Path<Uuid>,
    Form(form): Form<Action>,
) -> WebResult<Redirect> {
    check_csrf(&app, &form.csrf)?;
    let _guard = app.delivery.lock().await;
    app.store
        .retry(&id.to_string(), form.confirm.as_deref() == Some("yes"))
        .await
        .map_err(|e| WebError(StatusCode::CONFLICT, e.to_string()))?;
    Ok(Redirect::to(&format!("/jobs/{id}")))
}
async fn jobs(State(app): State<Arc<App>>) -> WebResult<Json<Vec<Job>>> {
    Ok(Json(app.store.recent().await?))
}

async fn resolve(
    State(app): State<Arc<App>>,
    Path(id): Path<Uuid>,
    Form(form): Form<Action>,
) -> WebResult<Redirect> {
    check_csrf(&app, &form.csrf)?;
    if form.confirm.as_deref() != Some("yes") {
        return Err(WebError(
            StatusCode::BAD_REQUEST,
            "Confirm that the existing document matches this scan".into(),
        ));
    }
    let document = form.document_id.ok_or_else(|| {
        WebError(
            StatusCode::BAD_REQUEST,
            "Enter the existing Paperless document ID".into(),
        )
    })?;
    app.reconcile(&id.to_string(), document)
        .await
        .map_err(|e| WebError(StatusCode::CONFLICT, e.to_string()))?;
    Ok(Redirect::to(&format!("/jobs/{id}")))
}
async fn job_api(State(app): State<Arc<App>>, Path(id): Path<Uuid>) -> WebResult<Json<Job>> {
    Ok(Json(find_job(&app, id).await?))
}
async fn find_job(app: &App, id: Uuid) -> WebResult<Job> {
    app.store
        .job(&id.to_string())
        .await?
        .ok_or_else(|| WebError(StatusCode::NOT_FOUND, "Job not found".into()))
}
async fn status(State(app): State<Arc<App>>) -> WebResult<Json<serde_json::Value>> {
    Ok(Json(
        serde_json::json!({ "observed": &*app.status.read().await, "scanner_uncertain": app.store.uncertain().await?, "button_source": app.config.button_source, "now": now() }),
    ))
}
async fn ready(State(app): State<Arc<App>>) -> StatusCode {
    if app.stop.is_cancelled()
        || app.store.queue_stats().await.is_err()
        || tokio::fs::metadata(app.store.root()).await.is_err()
    {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    }
}
async fn metrics(State(app): State<Arc<App>>) -> WebResult<Response> {
    Ok((
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        app.metrics.render()?,
    )
        .into_response())
}
async fn download(State(app): State<Arc<App>>, Path(id): Path<Uuid>) -> WebResult<Response> {
    let job = find_job(&app, id).await?;
    if job.sha256.is_none() || job.state == "deleted" {
        return Err(WebError(
            StatusCode::NOT_FOUND,
            "No retained complete PDF".into(),
        ));
    }
    let file = tokio::fs::File::open(app.store.directory(&id.to_string()).join("document.pdf"))
        .await
        .map_err(|_| WebError(StatusCode::NOT_FOUND, "PDF is missing".into()))?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/pdf"),
            (header::CONTENT_DISPOSITION, "attachment; filename=scan.pdf"),
        ],
        Body::from_stream(tokio_util::io::ReaderStream::new(file)),
    )
        .into_response())
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
fn timestamp(value: i64) -> String {
    chrono::DateTime::from_timestamp(value, 0)
        .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn page(title: &str, body: String) -> Html<String> {
    Html(format!(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>{}</title><style>body{{font:17px system-ui;max-width:960px;margin:2rem auto;padding:0 1rem;background:#f5f6f7;color:#18222b}}a{{color:#125c9d}}table{{border-collapse:collapse;width:100%;background:white}}td,th{{padding:.7rem;text-align:left;border-bottom:1px solid #ddd}}button,select{{font:inherit;padding:.5rem;margin:.3rem}}.issue{{background:#fff1ce;padding:1rem}}code{{overflow-wrap:anywhere}}dt{{font-weight:bold;margin-top:1rem}}dd{{margin:.2rem 0 1rem}}header{{display:flex;align-items:center;justify-content:space-between}}</style><header><h1><a href=\"/\">brscanr</a></h1><a href=\"/\">Refresh status</a></header><main>{body}</main></html>",
        escape(title)
    ))
}

async fn index(State(app): State<Arc<App>>) -> WebResult<Html<String>> {
    let status = app.status.read().await.clone();
    let uncertain = app.store.uncertain().await?;
    let jobs = app.store.recent().await?;
    let busy = jobs.iter().any(|job| job.state == "acquiring");
    let lease = match status.registration_accepted {
        Some(t) if now() < t + 360 => format!(
            "Registration accepted at {}; estimated expiry {}.",
            timestamp(t),
            timestamp(t + 360)
        ),
        Some(_) => "Registration lease expired or uncertain.".into(),
        None => "Waiting for registration acceptance.".into(),
    };
    let mut body = format!(
        "<h2>Scanner</h2><p>{}</p><p>Last validated callback: {}.</p><p>Physical button preset: {:?}, 300 dpi grayscale.</p>",
        escape(&lease),
        status
            .last_callback
            .map(timestamp)
            .unwrap_or_else(|| "not observed; registration alone does not verify routing".into()),
        app.config.button_source
    );
    for error in [status.registration_error, status.storage_error.clone()]
        .into_iter()
        .flatten()
    {
        body.push_str(&format!("<p class=\"issue\">{}</p>", escape(&error)));
    }
    if uncertain {
        body.push_str(&format!("<div class=\"issue\"><p>A previous scan was interrupted. Check the printer, clear any jam, and confirm it is idle before allowing another scan.</p><form method=\"post\" action=\"/scanner/acknowledge\"><input type=\"hidden\" name=\"csrf\" value=\"{}\"><button>I checked the printer; allow scans</button></form></div>", app.csrf));
    }
    let disabled = if busy || uncertain || status.storage_error.is_some() {
        "disabled"
    } else {
        ""
    };
    body.push_str(&format!("<h2>Start a scan</h2><p>Load the document before starting. Feeder scans combine all pages into one PDF.</p><form method=\"post\" action=\"/scan\"><input type=\"hidden\" name=\"csrf\" value=\"{}\"><input type=\"hidden\" name=\"idempotency_key\" value=\"{}\"><label>Source <select name=\"source\"><option value=\"feeder\">Feeder</option><option value=\"platen\">Platen</option></select></label><button {disabled}>Scan at 300 dpi</button></form><h2>Recent scans</h2><p>Complete scans stay in the local spool until Paperless confirms import. Delivery retries never operate the scanner.</p><table><tr><th>Started</th><th>Source</th><th>Pages</th><th>Status</th></tr>", app.csrf, Uuid::new_v4()));
    for job in jobs {
        body.push_str(&format!(
            "<tr><td><a href=\"/jobs/{}\">{}</a></td><td>{} ({})</td><td>{}</td><td>{}</td></tr>",
            job.id,
            timestamp(job.created),
            escape(&job.source),
            escape(&job.trigger),
            job.pages,
            escape(&job.state)
        ));
    }
    body.push_str("</table>");
    Ok(page("brscanr", body))
}

async fn job_page(State(app): State<Arc<App>>, Path(id): Path<Uuid>) -> WebResult<Html<String>> {
    let job = find_job(&app, id).await?;
    let mut body = format!(
        "<h2>Scan</h2><dl><dt>Job</dt><dd><code>{}</code></dd><dt>Status</dt><dd>{}</dd><dt>Pages</dt><dd>{}</dd><dt>Source</dt><dd>{}</dd><dt>Started / updated</dt><dd>{} / {}</dd></dl>",
        job.id,
        escape(&job.state),
        job.pages,
        escape(&job.source),
        timestamp(job.created),
        timestamp(job.updated)
    );
    if let Some(error) = job.error {
        body.push_str(&format!("<p class=\"issue\">{}</p>", escape(&error)));
    }
    if let Some(task) = job.task_id {
        body.push_str(&format!(
            "<p>Paperless task: <code>{}</code></p>",
            escape(&task)
        ));
    }
    if let Some(document) = job.document_id {
        let url = app
            .config
            .paperless_url
            .join(&format!("documents/{document}/"))
            .map_err(anyhow::Error::from)?;
        body.push_str(&format!(
            "<p><a href=\"{}\">Open Paperless document {document}</a></p>",
            escape(url.as_str())
        ));
    }
    if let Some(hash) = job.sha256
        && job.state != "deleted"
    {
        body.push_str(&format!("<p><a href=\"/jobs/{id}/document.pdf\">Download retained PDF</a></p><p>SHA256: <code>{}</code></p>", escape(&hash)));
    }
    if job.state == "unknown" || job.state == "delivery_failed" {
        let confirm = if job.state == "unknown" {
            "<p>Check Paperless first. Retrying an unknown outcome can create a duplicate.</p><label><input type=\"checkbox\" name=\"confirm\" value=\"yes\" required>I checked Paperless and accept the duplicate risk.</label>"
        } else {
            ""
        };
        body.push_str(&format!("<form method=\"post\" action=\"/jobs/{id}/retry\"><input type=\"hidden\" name=\"csrf\" value=\"{}\">{confirm}<button>Retry delivery</button></form>", app.csrf));
    }
    if job.state == "unknown" {
        body.push_str(&format!("<h3>Found this scan in Paperless?</h3><form method=\"post\" action=\"/jobs/{id}/resolve\"><input type=\"hidden\" name=\"csrf\" value=\"{}\"><p><label>Existing Paperless document ID <input type=\"number\" name=\"document_id\" min=\"1\" required></label></p><p><label><input type=\"checkbox\" name=\"confirm\" value=\"yes\" required>I verified that this Paperless document contains this scan.</label></p><button>Record existing import</button></form>", app.csrf));
    }
    body.push_str("<p><a href=\"/\">Start a new scan or check scanner status</a></p>");
    Ok(page("Scan details — brscanr", body))
}
