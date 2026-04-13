use anyhow::Result;
use async_trait::async_trait;
use base64::Engine as _;
use bytes::Bytes;
use http::{header::CONTENT_TYPE, Request, StatusCode};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::task::JoinSet;
use tokio::time::{interval, Duration};
use tracing::{debug, error, warn};
use upload_response::{
    discover_ingress_origins, RemoteIngressClient, RemoteRequestSlot, RequestControl,
    ResponseCacheWriter,
};
use uuid::Uuid;
use web_service::HandlerResponse;

use crate::config::AppConfig;
use crate::encode::{
    headers_to_json, parse_request, process_parsed_request, process_request, response_headers,
    stream_artifact_selection, EncodeArtifacts, ProgressSink, StreamArtifactSelection,
};
use crate::protocol::{INTERNAL_STREAMING_MODE_HEADER, INTERNAL_STREAMING_MODE_JSONL};

const STREAM_ARTIFACT_CHUNK_BYTES: usize = 48 * 1024;
const STREAM_UPLOAD_PROGRESS_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub struct WorkerState {
    config: AppConfig,
}

struct JsonLineResponseWriter {
    writer: ResponseCacheWriter,
    started: bool,
    finished: bool,
}

impl WorkerState {
    pub fn new(config: AppConfig) -> Self {
        Self { config }
    }

    pub fn spawn_remote_cache_worker(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run_remote_cache_worker().await;
        })
    }

    async fn run_remote_cache_worker(self: Arc<Self>) {
        let client = match RemoteIngressClient::new(
            self.config.upload_response_config().slot_bytes(),
            self.config.upload_response_insecure_tls,
        ) {
            Ok(client) => client,
            Err(error) => {
                error!(error = %error, "failed to build remote ingress client");
                return;
            }
        };

        let mut poll = interval(Duration::from_millis(
            self.config.upload_response_worker_poll_ms.max(1),
        ));
        let mut discovery = interval(Duration::from_millis(
            self.config.upload_response_discovery_interval_ms.max(1),
        ));
        let mut inflight = HashSet::new();
        let mut tasks = JoinSet::new();
        let mut origins: Vec<String> = Vec::new();
        let mut refresh_origins = true;

        loop {
            tokio::select! {
                _ = poll.tick() => {}
                _ = discovery.tick() => {
                    refresh_origins = true;
                }
            }

            if refresh_origins {
                match discover_ingress_origins(
                    &self.config.upload_response_ingress_urls,
                    self.config.upload_response_discovery_dns.as_deref(),
                )
                .await
                {
                    Ok(next) => {
                        if next != origins {
                            debug!(origins = ?next, "updated encode ingress origins");
                        }
                        origins = next;
                    }
                    Err(error) => {
                        warn!(error = %error, "failed to discover ingress origins");
                    }
                }
                refresh_origins = false;
            }

            while let Some(joined) = tasks.try_join_next() {
                match joined {
                    Ok(key) => {
                        inflight.remove(&key);
                    }
                    Err(error) => {
                        error!(%error, "remote encode worker task failed");
                    }
                }
            }

            if inflight.len() >= self.config.upload_response_max_inflight {
                continue;
            }

            for origin in &origins {
                if inflight.len() >= self.config.upload_response_max_inflight {
                    break;
                }

                let streams = match client.list_streams(origin).await {
                    Ok(streams) => streams,
                    Err(error) => {
                        warn!(origin, error = %error, "failed to list remote streams");
                        continue;
                    }
                };

                for stream in streams {
                    if inflight.len() >= self.config.upload_response_max_inflight {
                        break;
                    }
                    if stream.request_last == 0 || stream.response_owner.is_some() {
                        continue;
                    }

                    let inflight_key = format!("{}#{}", origin, stream.stream_id);
                    if inflight.contains(&inflight_key) {
                        continue;
                    }

                    match client
                        .try_claim_response(
                            origin,
                            stream.stream_id,
                            &self.config.upload_response_worker_id,
                        )
                        .await
                    {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(error) => {
                            warn!(
                                origin,
                                stream_id = stream.stream_id,
                                error = %error,
                                "failed to claim remote encode response"
                            );
                            continue;
                        }
                    }

                    let _ = client
                        .register_reader(
                            origin,
                            stream.stream_id,
                            &self.config.upload_response_worker_id,
                        )
                        .await;

                    inflight.insert(inflight_key.clone());
                    let worker = self.clone();
                    let client = client.clone();
                    let worker_id = self.config.upload_response_worker_id.clone();
                    let origin = origin.clone();
                    tasks.spawn(async move {
                        let result = worker
                            .process_remote_stream(&client, &origin, stream.stream_id)
                            .await;
                        if let Err(error) = result {
                            error!(
                                origin,
                                stream_id = stream.stream_id,
                                error = %error,
                                "remote cached encode failed"
                            );
                            let response =
                                error_response(classify_error(&error), error.to_string());
                            if let Err(write_error) = client
                                .write_handler_response(&origin, stream.stream_id, response)
                                .await
                            {
                                error!(
                                    origin,
                                    stream_id = stream.stream_id,
                                    error = %write_error,
                                    "failed to write remote cached error response"
                                );
                                let _ = client
                                    .release_response(&origin, stream.stream_id, &worker_id)
                                    .await;
                            }
                        }

                        let _ = client
                            .unregister_reader(&origin, stream.stream_id, &worker_id)
                            .await;
                        inflight_key
                    });
                }
            }
        }
    }

    async fn process_remote_stream(
        &self,
        client: &RemoteIngressClient,
        origin: &str,
        stream_id: u64,
    ) -> Result<()> {
        let request = client
            .request_headers(origin, stream_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("request headers missing for stream {stream_id}"))?;

        if is_streaming_request(&request) {
            let mut writer = JsonLineResponseWriter::remote(
                client.clone(),
                origin.to_string(),
                stream_id,
                client.slot_bytes(),
            );
            return self
                .stream_remote_upload(client, origin, stream_id, request, &mut writer)
                .await;
        }

        let body = self
            .read_remote_request_body(client, origin, stream_id)
            .await?;
        let mut progress: Option<&mut (dyn ProgressSink + Send)> = None;
        let artifacts = process_request(&self.config, &request, body, &mut progress).await?;
        let headers = response_headers(&artifacts);
        let response = if request.uri().path() == "/encode/ecdc" {
            HandlerResponse {
                status: StatusCode::OK,
                body: Some(artifacts.ecdc),
                content_type: Some("application/octet-stream".into()),
                headers: {
                    let mut headers = headers;
                    headers.push((
                        "content-disposition".into(),
                        "attachment; filename=\"output.ecdc\"".into(),
                    ));
                    headers
                },
                etag: None,
            }
        } else {
            HandlerResponse {
                status: StatusCode::OK,
                body: Some(artifacts.png),
                content_type: Some("image/png".into()),
                headers,
                etag: None,
            }
        };
        client
            .write_handler_response(origin, stream_id, response)
            .await?;
        Ok(())
    }

    async fn stream_remote_upload(
        &self,
        client: &RemoteIngressClient,
        origin: &str,
        stream_id: u64,
        request: Request<()>,
        writer: &mut JsonLineResponseWriter,
    ) -> Result<()> {
        let request_id = Uuid::new_v4().to_string();
        let artifact_selection = stream_artifact_selection(&request)?;

        writer
            .send_value(json!({
                "type": "Metadata",
                "request_id": request_id.as_str(),
                "path": request.uri().path(),
                "artifact": artifact_selection.as_str(),
            }))
            .await?;

        let body = self
            .read_remote_streaming_request_body(client, origin, stream_id, writer, &request_id)
            .await?;
        let parsed = parse_request(&request, body).await?;

        writer
            .send_value(json!({
                "type": "Request",
                "request_id": request_id.as_str(),
                "track_count": parsed.files.len(),
                "quality": parsed.quality.as_str(),
                "gap_seconds": parsed.gap_seconds,
                "audio_key": parsed.audio_key.as_str(),
            }))
            .await?;

        let mut progress: Option<&mut (dyn ProgressSink + Send)> = Some(writer);
        let artifacts = process_parsed_request(&self.config, parsed, &mut progress).await?;
        self.send_stream_artifacts(writer, &request_id, artifact_selection, &artifacts)
            .await?;
        writer
            .send_value(json!({
                "type": "Done",
                "request_id": request_id.as_str(),
                "cached": false,
                "meta": headers_to_json(&response_headers(&artifacts)),
                "duration": artifacts.duration_seconds,
                "png_size": artifacts.png_side,
                "ecdc_bytes": artifacts.ecdc.len(),
            }))
            .await?;
        writer.finish().await
    }

    async fn read_remote_streaming_request_body(
        &self,
        client: &RemoteIngressClient,
        origin: &str,
        stream_id: u64,
        writer: &mut JsonLineResponseWriter,
        request_id: &str,
    ) -> Result<Bytes> {
        let mut poll = interval(Duration::from_millis(
            self.config.upload_response_worker_poll_ms.max(1),
        ));
        let mut last_slot = 1usize;
        let mut buffer = Vec::new();
        let mut last_reported_bytes = 0usize;

        'stream: loop {
            poll.tick().await;
            let current_last = client.request_last(origin, stream_id).await?;
            if current_last <= last_slot {
                continue;
            }

            for slot_id in (last_slot + 1)..=current_last {
                match client.request_slot(origin, stream_id, slot_id).await? {
                    Some(RemoteRequestSlot::Body(bytes)) => {
                        buffer.extend_from_slice(&bytes);
                        if buffer.len().saturating_sub(last_reported_bytes)
                            >= STREAM_UPLOAD_PROGRESS_BYTES
                        {
                            last_reported_bytes = buffer.len();
                            writer
                                .send_value(json!({
                                    "type": "UploadProgress",
                                    "request_id": request_id,
                                    "bytes_received": buffer.len(),
                                }))
                                .await?;
                        }
                    }
                    Some(RemoteRequestSlot::Control(RequestControl::Finalize)) => {
                        writer
                            .send_value(json!({
                                "type": "UploadProgress",
                                "request_id": request_id,
                                "bytes_received": buffer.len(),
                                "finalize": true,
                            }))
                            .await?;
                    }
                    Some(RemoteRequestSlot::Control(RequestControl::KeepAlive)) => {}
                    Some(RemoteRequestSlot::End) => break 'stream,
                    Some(RemoteRequestSlot::Headers(_)) | None => {}
                }
            }

            last_slot = current_last;
        }

        anyhow::ensure!(
            !buffer.is_empty(),
            "request body did not include audio bytes"
        );

        writer
            .send_value(json!({
                "type": "UploadComplete",
                "request_id": request_id,
                "bytes_received": buffer.len(),
            }))
            .await?;

        Ok(Bytes::from(buffer))
    }

    async fn read_remote_request_body(
        &self,
        client: &RemoteIngressClient,
        origin: &str,
        stream_id: u64,
    ) -> Result<Bytes> {
        let mut poll = interval(Duration::from_millis(
            self.config.upload_response_worker_poll_ms.max(1),
        ));
        let mut last_slot = 1usize;
        let mut buffer = Vec::new();

        'stream: loop {
            poll.tick().await;
            let current_last = client.request_last(origin, stream_id).await?;
            if current_last <= last_slot {
                continue;
            }

            for slot_id in (last_slot + 1)..=current_last {
                match client.request_slot(origin, stream_id, slot_id).await? {
                    Some(RemoteRequestSlot::Body(bytes)) => buffer.extend_from_slice(&bytes),
                    Some(RemoteRequestSlot::Control(_)) => {}
                    Some(RemoteRequestSlot::End) => break 'stream,
                    Some(RemoteRequestSlot::Headers(_)) | None => {}
                }
            }

            last_slot = current_last;
        }

        Ok(Bytes::from(buffer))
    }

    async fn send_stream_artifacts(
        &self,
        writer: &mut JsonLineResponseWriter,
        request_id: &str,
        selection: StreamArtifactSelection,
        artifacts: &EncodeArtifacts,
    ) -> Result<()> {
        if selection.includes_ecdc() {
            send_artifact_chunks(
                writer,
                request_id,
                "ecdc",
                "application/octet-stream",
                &artifacts.ecdc,
            )
            .await?;
        }

        if selection.includes_png() {
            send_artifact_chunks(writer, request_id, "png", "image/png", &artifacts.png).await?;
        }

        Ok(())
    }
}

#[async_trait]
impl ProgressSink for JsonLineResponseWriter {
    async fn send(&mut self, payload: Value) -> Result<()> {
        self.send_value(payload).await
    }
}

impl JsonLineResponseWriter {
    fn remote(
        client: RemoteIngressClient,
        origin: String,
        stream_id: u64,
        slot_bytes: usize,
    ) -> Self {
        Self {
            writer: ResponseCacheWriter::remote(client, origin, stream_id, slot_bytes),
            started: false,
            finished: false,
        }
    }

    async fn send_value(&mut self, value: Value) -> Result<()> {
        self.ensure_started().await?;
        let mut payload = serde_json::to_vec(&value)
            .map_err(|error| anyhow::anyhow!("failed to serialize streaming event: {error}"))?;
        payload.push(b'\n');
        self.writer.send_body(Bytes::from(payload)).await
    }

    async fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.ensure_started().await?;
        self.writer.finish().await?;
        self.finished = true;
        Ok(())
    }

    async fn ensure_started(&mut self) -> Result<()> {
        if self.started {
            return Ok(());
        }

        let response_head = http::Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "application/x-ndjson; charset=utf-8")
            .header("cache-control", "no-store")
            .body(())
            .map_err(|error| anyhow::anyhow!("failed to build streaming response head: {error}"))?;
        self.writer.ensure_started(response_head).await?;
        self.started = true;
        Ok(())
    }
}

async fn send_artifact_chunks(
    writer: &mut JsonLineResponseWriter,
    request_id: &str,
    name: &str,
    content_type: &str,
    bytes: &[u8],
) -> Result<()> {
    writer
        .send_value(json!({
            "type": "ArtifactStart",
            "request_id": request_id,
            "name": name,
            "content_type": content_type,
            "encoding": "base64",
            "size_bytes": bytes.len(),
        }))
        .await?;

    let mut chunks = 0usize;
    for (index, chunk) in bytes.chunks(STREAM_ARTIFACT_CHUNK_BYTES).enumerate() {
        chunks = index + 1;
        writer
            .send_value(json!({
                "type": "ArtifactChunk",
                "request_id": request_id,
                "name": name,
                "index": index,
                "data": base64::engine::general_purpose::STANDARD.encode(chunk),
            }))
            .await?;
    }

    writer
        .send_value(json!({
            "type": "ArtifactEnd",
            "request_id": request_id,
            "name": name,
            "chunks": chunks,
        }))
        .await?;

    Ok(())
}

fn is_streaming_request(req: &Request<()>) -> bool {
    req.headers()
        .get(INTERNAL_STREAMING_MODE_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.eq_ignore_ascii_case(INTERNAL_STREAMING_MODE_JSONL))
        .unwrap_or(false)
}

fn error_response(status: StatusCode, message: String) -> HandlerResponse {
    HandlerResponse {
        status,
        body: Some(Bytes::from(format!(
            "{{\"error\":{}}}",
            serde_json::to_string(&message).unwrap_or_else(|_| "\"internal error\"".into())
        ))),
        content_type: Some("application/json".into()),
        headers: vec![("cache-control".into(), "no-store".into())],
        etag: None,
    }
}

fn classify_error(error: &anyhow::Error) -> StatusCode {
    if crate::encode::is_client_error(error) {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    }
}
