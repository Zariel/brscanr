mod config;
mod metrics;
mod paperless;
mod service;
mod store;
mod web;

use std::{fs::OpenOptions, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use brother_scan::Registration;
use fs2::FileExt;
use tokio::{
    net::{TcpListener, UdpSocket},
    task::JoinSet,
};

use crate::{config::Config, service::App, store::Store};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "brscanr=info".into()),
        )
        .init();
    let config = Config::from_env()?;
    tokio::fs::create_dir_all(&config.spool).await?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(config.spool.join("owner.lock"))?;
    // Hold for the entire process lifetime. Recreate rollouts alone cannot fence an old owner.
    lock.try_lock_exclusive()
        .context("another process owns this spool")?;
    let registration = Registration::new(
        config.printer,
        config.callback,
        config.destination.clone(),
        config.insecure_tls,
    )?;
    // Bind before advertising. Readiness must not depend on callback reachability.
    let socket = UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, config.callback.port())).await?;
    let listener = TcpListener::bind(config.listen).await?;
    let store = Store::open(&config.spool).await?;
    service::sync_directory(&config.spool).await?;
    let app = App::new(config, store)?;
    tracing::info!(address=%listener.local_addr()?, "brscanr listening");

    let mut workers = JoinSet::new();
    workers.spawn(Arc::clone(&app).registrations(registration));
    workers.spawn(Arc::clone(&app).notifications(socket));
    workers.spawn(Arc::clone(&app).deliveries());
    let stopped = app.stop.clone();
    let router = web::router(Arc::clone(&app));
    workers.spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(stopped.cancelled_owned())
            .await?;
        Ok(())
    });
    let graceful = tokio::select! {
        _ = shutdown_signal() => true,
        _ = app.stop.cancelled() => false,
        result = workers.join_next() => {
            tracing::error!(?result, "core worker exited");
            false
        }
    };
    app.stop.cancel();
    app.tasks.close();
    let drained = tokio::time::timeout(Duration::from_secs(20), async {
        while workers.join_next().await.is_some() {}
        app.tasks.wait().await;
    })
    .await;
    if drained.is_err() {
        bail!("shutdown deadline exceeded; in-flight jobs will be reconciled on restart");
    }
    if !graceful {
        bail!("service worker failed");
    }
    drop(lock);
    Ok(())
}

async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
}
