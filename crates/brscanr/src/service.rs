use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Result, bail};
use brother_scan::{Capture, Notification, Registration, Scanner, Source};
use sha2::{Digest, Sha256};
use tokio::{
    net::UdpSocket,
    sync::{Mutex, RwLock},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{
    config::Config,
    metrics::Metrics,
    paperless::{Outcome, Paperless, Upload},
    store::{Admission, Job, Store, now},
};

#[derive(Default, Clone, serde::Serialize)]
pub struct Status {
    pub registration_accepted: Option<i64>,
    pub registration_error: Option<String>,
    pub last_callback: Option<i64>,
    pub storage_error: Option<String>,
}

pub struct App {
    pub config: Config,
    pub store: Store,
    pub status: RwLock<Status>,
    pub metrics: Metrics,
    pub stop: CancellationToken,
    pub tasks: TaskTracker,
    pub delivery: Mutex<()>,
    pub admission: Mutex<()>,
    pub csrf: String,
    paperless: Paperless,
}

impl App {
    pub fn new(config: Config, store: Store) -> Result<Arc<Self>> {
        let paperless = Paperless::new(config.paperless_url.clone(), &config.paperless_token)?;
        Ok(Arc::new(Self {
            config,
            store,
            status: RwLock::new(Status::default()),
            metrics: Metrics::new()?,
            stop: CancellationToken::new(),
            tasks: TaskTracker::new(),
            delivery: Mutex::new(()),
            admission: Mutex::new(()),
            csrf: uuid::Uuid::new_v4().to_string(),
            paperless,
        }))
    }

    pub async fn submit(
        self: &Arc<Self>,
        key: &str,
        source: Source,
        trigger: &str,
    ) -> Result<Admission> {
        let _guard = self.admission.lock().await;
        if self.stop.is_cancelled() {
            bail!("service is shutting down");
        }
        let storage = self.storage().await.err().map(|e| e.to_string());
        self.status.write().await.storage_error = storage.clone();
        let result = self
            .store
            .admit(key, source, trigger, storage.as_deref())
            .await?;
        if let Admission::Started(job) = &result {
            let app = Arc::clone(self);
            let id = job.id.clone();
            self.tasks.spawn(async move {
                if let Err(error) = app.capture(&id, source).await {
                    app.fatal(error);
                }
            });
        }
        Ok(result)
    }

    async fn capture(&self, id: &str, source: Source) -> Result<()> {
        let directory = self.store.directory(id);
        let operation = async {
            tokio::fs::create_dir(&directory).await?;
            sync_directory(self.store.root()).await?;
            let scanner = Scanner::new(
                (self.config.printer, 54921).into(),
                self.config.limits.clone(),
            );
            let capture = scanner.scan(source, &directory).await?;
            // Publish capture evidence only after all page files and their directory are durable.
            sync_directory(&directory).await?;
            publish(
                &directory.join("capture.json"),
                &serde_json::to_vec(&capture)?,
            )
            .await?;
            Ok::<_, anyhow::Error>(capture)
        };
        let result = tokio::select! {
            _ = self.stop.cancelled() => Err(anyhow::anyhow!("Service stopped during acquisition")),
            result = operation => result,
        };
        match result {
            Ok(capture) => {
                self.store.captured(id, capture.pages.len()).await?;
                self.metrics.captured.inc();
                tracing::info!(job_id = id, pages = capture.pages.len(), "scan captured");
            }
            Err(error) => {
                self.store.interrupted(id, &error.to_string()).await?;
                self.metrics.failed.inc();
                tracing::warn!(job_id=id, %error, "acquisition interrupted; explicit acknowledgment required");
            }
        }
        Ok(())
    }

    pub fn fatal(&self, error: anyhow::Error) {
        tracing::error!(%error, "service worker failed");
        self.stop.cancel();
    }

    pub async fn storage(&self) -> Result<u64> {
        let root = self.config.spool.clone();
        let (used, free) = tokio::task::spawn_blocking(move || -> Result<_> {
            Ok((disk_usage(&root)?, fs2::available_space(&root)?))
        })
        .await??;
        self.metrics
            .spool_bytes
            .set(used.min(i64::MAX as u64) as i64);
        // Allow space for raw capture, assembled PDF, and one concurrent assembly.
        let reserve = self.config.limits.max_bytes.saturating_mul(3);
        if used.saturating_add(reserve) > self.config.max_spool_bytes {
            bail!("Spool limit reached; resolve retained jobs or increase storage");
        }
        if free < self.config.min_free_bytes.saturating_add(reserve) {
            bail!("Insufficient free spool space");
        }
        Ok(used)
    }

    pub async fn registrations(self: Arc<Self>, registration: Registration) -> Result<()> {
        let mut delay = Duration::ZERO;
        let mut failures = 0u32;
        loop {
            tokio::select! { _ = self.stop.cancelled() => return Ok(()), _ = tokio::time::sleep(delay) => {} }
            let result = tokio::select! { _ = self.stop.cancelled() => return Ok(()), result = registration.renew() => result };
            let mut status = self.status.write().await;
            match result {
                Ok(()) => {
                    let accepted = now();
                    status.registration_accepted = Some(accepted);
                    status.registration_error = None;
                    self.metrics.lease_until.set(accepted + 360);
                    failures = 0;
                    // Jitter avoids synchronized renewals after fleet restarts.
                    delay = Duration::from_secs(
                        115 + u64::from(uuid::Uuid::new_v4().as_bytes()[0] % 11),
                    );
                    tracing::info!("printer registration renewed");
                }
                Err(error) => {
                    status.registration_error = Some(error.to_string());
                    self.metrics.renew_failures.inc();
                    failures = failures.saturating_add(1);
                    delay = Duration::from_secs((1u64 << failures.min(6)).min(60));
                    tracing::warn!(%error, "registration failed; will retry");
                }
            }
        }
    }

    pub async fn notifications(self: Arc<Self>, socket: UdpSocket) -> Result<()> {
        let mut packet = [0u8; 4097];
        loop {
            let (size, peer) = tokio::select! {
                _ = self.stop.cancelled() => return Ok(()),
                received = socket.recv_from(&mut packet) => received?,
            };
            if peer.ip() != self.config.printer {
                continue;
            }
            let Ok(event) = Notification::parse(
                &packet[..size],
                &self.config.destination,
                &self.config.callback.to_string(),
            ) else {
                continue;
            };
            self.status.write().await.last_callback = Some(now());
            self.metrics.last_callback.set(now());
            let key = format!(
                "button:{}:{}:{}:{}:{}:{}",
                peer.ip(),
                self.config.callback,
                self.config.destination,
                event.function,
                event.registration_id,
                event.sequence
            );
            match self
                .submit(&key, self.config.button_source, "button")
                .await?
            {
                Admission::Started(job) => tracing::info!(job_id = job.id, "button scan admitted"),
                Admission::Existing(_) => tracing::debug!("duplicate button event"),
                Admission::Rejected(reason) => tracing::warn!(%reason, "button scan rejected"),
            }
        }
    }

    pub async fn deliveries(self: Arc<Self>) -> Result<()> {
        loop {
            tokio::select! { _ = self.stop.cancelled() => return Ok(()), _ = tokio::time::sleep(Duration::from_secs(5)) => {} }
            let _guard = self.delivery.lock().await;
            for job in self.store.pending().await? {
                tokio::select! {
                    _ = self.stop.cancelled() => return Ok(()),
                    result = self.deliver(job) => result?,
                }
            }
            self.cleanup().await?;
            let (count, oldest) = self.store.queue_stats().await?;
            self.metrics.queue.set(count);
            self.metrics.oldest.set(oldest);
            self.status.write().await.storage_error =
                self.storage().await.err().map(|e| e.to_string());
        }
    }

    async fn deliver(&self, job: Job) -> Result<()> {
        let directory = self.store.directory(&job.id);
        let pdf = directory.join("document.pdf");
        match job.state.as_str() {
            "captured" => {
                let result = async {
                    let capture: Capture = serde_json::from_slice(
                        &tokio::fs::read(directory.join("capture.json")).await?,
                    )?;
                    let staged = directory.join("document.pdf.part");
                    let output = staged.clone();
                    // Only the delivery worker owns this staging path; a crash may leave it behind.
                    if staged.exists() {
                        tokio::fs::remove_file(&staged).await?;
                    }
                    tokio::task::spawn_blocking(move || {
                        brother_scan::assemble_pdf(&capture, &output)
                    })
                    .await??;
                    tokio::fs::rename(staged, &pdf).await?;
                    sync_directory(&directory).await?;
                    let hash = hash_file(&pdf).await?;
                    Ok::<_, anyhow::Error>(hash)
                }
                .await;
                match result {
                    Ok(hash) => self.store.ready(&job.id, &hash).await?,
                    Err(error) => {
                        self.store
                            .transition(
                                &job.id,
                                "failed",
                                Some(&format!("PDF assembly failed: {error}; raw pages retained")),
                                None,
                                None,
                            )
                            .await?
                    }
                }
            }
            "ready" => {
                if hash_file(&pdf).await.ok().as_deref() != job.sha256.as_deref()
                    || job.sha256.is_none()
                {
                    self.store
                        .transition(
                            &job.id,
                            "failed",
                            Some("PDF is missing or changed; inspect spool"),
                            None,
                            None,
                        )
                        .await?;
                    return Ok(());
                }
                // Persist the ambiguity boundary before any request can reach Paperless.
                self.store
                    .transition(&job.id, "uploading", None, None, None)
                    .await?;
                let outcome = self.paperless.upload(&job.id, &pdf).await;
                match outcome {
                    Ok(Upload::Submitted(task)) => {
                        self.store
                            .transition(&job.id, "submitted", None, Some(&task), None)
                            .await?
                    }
                    Ok(Upload::Retry) => {
                        self.store
                            .defer(&job, "ready", "Paperless unavailable; delivery will retry")
                            .await?
                    }
                    Ok(Upload::Rejected(error)) => {
                        self.store
                            .transition(&job.id, "delivery_failed", Some(&error), None, None)
                            .await?
                    }
                    Ok(Upload::Unknown(error)) => {
                        self.store
                            .transition(&job.id, "unknown", Some(&error), None, None)
                            .await?
                    }
                    Err(error) => {
                        self.store
                            .transition(
                                &job.id,
                                "delivery_failed",
                                Some(&format!("Cannot prepare upload: {error}")),
                                None,
                                None,
                            )
                            .await?
                    }
                }
            }
            "submitted" => {
                match self.paperless.poll(job.task_id.as_deref().unwrap_or_default()).await {
                    Ok(Outcome::Imported(document)) => {
                        self.store.transition(&job.id, "imported", None, None, Some(document)).await?;
                        tracing::info!(job_id=job.id, document_id=document, "Paperless import completed");
                    }
                    Ok(Outcome::Failed) => self.store.transition(&job.id, "delivery_failed", Some("Paperless processing failed; inspect Paperless before retrying delivery"), None, None).await?,
                    Ok(Outcome::Missing) => self.store.transition(&job.id, "unknown", Some("Paperless task disappeared; reconcile before retrying"), None, None).await?,
                    Ok(Outcome::Pending) => {},
                    Err(_) => self.store.defer(&job, "submitted", "Cannot read Paperless task; polling will retry").await?,
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn cleanup(&self) -> Result<()> {
        let seconds = self
            .config
            .retention_days
            .saturating_mul(86400)
            .min(i64::MAX as u64) as i64;
        for job in self.store.expired(now().saturating_sub(seconds)).await? {
            let directory = self.store.directory(&job.id);
            // Imported jobs alone can be deleted. Repeating removal after a crash is safe.
            match tokio::fs::remove_dir_all(&directory).await {
                Ok(()) => sync_directory(self.store.root()).await?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(job_id=job.id, %error, "retention cleanup failed");
                    continue;
                }
            }
            self.store
                .transition(&job.id, "deleted", None, None, None)
                .await?;
        }
        Ok(())
    }
}

pub async fn sync_directory(path: &Path) -> Result<()> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || std::fs::File::open(path)?.sync_all()).await??;
    Ok(())
}

async fn publish(path: &Path, bytes: &[u8]) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let temporary = path.with_extension("part");
    let mut file = tokio::fs::File::create_new(&temporary).await?;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(temporary, path).await?;
    sync_directory(path.parent().unwrap()).await
}

async fn hash_file(path: &Path) -> Result<String> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path).await?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn disk_usage(root: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            total = total.saturating_add(disk_usage(&entry.path())?);
        } else if metadata.is_file() {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}
