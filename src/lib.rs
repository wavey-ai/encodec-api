pub mod config;
pub mod encode;
pub mod ingress;
pub mod protocol;
pub mod router;
pub mod worker;

use anyhow::{Context, Result};
use clap::Parser;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::EnvFilter;
use upload_response::{ResponseWatcher, UploadResponseRouter, UploadResponseService};
use web_service::{H2H3Server, Server, ServerBuilder};

use crate::config::AppConfig;
use crate::ingress::{EncodeIngress, EncodeIngressWebSocketHandler};
use crate::router::AppRouter;
use crate::worker::WorkerState;

pub fn init_tracing(rust_log: &str) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(rust_log)),
        )
        .try_init();
}

pub async fn run(config: AppConfig) -> Result<()> {
    config.validate()?;

    let (cert_b64, key_b64) = config.tls_base64()?;

    let upload_service = if config.role.exposes_upload_cache() {
        Some(Arc::new(UploadResponseService::new(
            config.upload_response_config(),
        )))
    } else {
        None
    };

    let _watcher_handle = upload_service.as_ref().map(|service| {
        ResponseWatcher::new(service.clone())
            .with_poll_interval_ms(config.upload_response_watch_poll_ms)
            .spawn()
    });

    let worker_state = Arc::new(WorkerState::new(config.clone()));
    let _worker_handle = if config.role.runs_remote_worker() {
        Some(worker_state.spawn_remote_cache_worker())
    } else {
        None
    };

    let upload_router = upload_service
        .as_ref()
        .map(|service| Arc::new(UploadResponseRouter::new(service.clone())));
    let encode_ingress = upload_service
        .as_ref()
        .filter(|_| config.role.serves_encode())
        .map(|service| Arc::new(EncodeIngress::new(config.clone(), service.clone())));
    let encode_ws = encode_ingress
        .as_ref()
        .map(|ingress| Arc::new(EncodeIngressWebSocketHandler::new(ingress.clone())));

    let router = Box::new(AppRouter::new(
        config.clone(),
        upload_router,
        encode_ingress,
        encode_ws.clone(),
    ));

    let enable_websocket = encode_ws.is_some();

    let server = H2H3Server::builder()
        .with_tls(cert_b64, key_b64)
        .with_port(config.port)
        .enable_h2(true)
        .enable_h3(config.enable_h3)
        .enable_websocket(enable_websocket)
        .with_router(router)
        .build()
        .context("failed to build encodec-api server")?;

    let handle = server.start().await.context("failed to start server")?;
    let _ = handle.ready_rx.await;
    info!(
        role = ?config.role,
        port = config.port,
        enable_h3 = config.enable_h3,
        ffmpeg_bin = %config.ffmpeg_bin,
        encodec_bin = %config.encodec_bin,
        encodec_python = %config
            .encodec_python
            .as_deref()
            .unwrap_or("-"),
        upload_response_num_streams = config.upload_response_num_streams,
        upload_response_slot_size_kb = config.upload_response_slot_size_kb,
        upload_response_slots_per_stream = config.upload_response_slots_per_stream,
        upload_response_worker_id = %config.upload_response_worker_id,
        upload_response_ingress_urls = ?config.upload_response_ingress_urls,
        upload_response_discovery_dns = ?config.upload_response_discovery_dns,
        "encodec-api ready"
    );

    tokio::signal::ctrl_c().await?;
    let _ = handle.shutdown_tx.send(());
    let _ = handle.finished_rx.await;
    Ok(())
}

pub async fn run_from_env() -> Result<()> {
    dotenvy::dotenv().ok();
    let config = AppConfig::parse();
    init_tracing(&config.rust_log);
    run(config).await
}
