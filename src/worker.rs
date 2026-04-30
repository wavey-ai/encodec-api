use anyhow::Result;
use async_trait::async_trait;
use base64::Engine as _;
use bytes::Bytes;
use gpu_worker_upload_response::{
    run_remote_worker_loop, PipelineSpec, RemoteJob, RemoteJobProcessor, RemoteWorkerConfig,
    SinkLane, SourceFrame, SourceLane,
};
use http::{header::CONTENT_TYPE, Request, StatusCode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::time::Duration;
use tracing::{debug, error, info};
use upload_response::{RemoteIngressClient, RequestControl, ResponseCacheWriter};
use uuid::Uuid;
use web_service::HandlerResponse;

use crate::config::AppConfig;
use crate::encode::{
    build_artifacts_from_ecdc, finalize_prepared_audio_key_digest, headers_to_json, parse_request,
    pcm_duration_seconds, prepared_program_from_internal_request,
    prepared_request_envelope_from_internal_request, process_parsed_request,
    process_prepared_program, process_request, response_headers,
    start_incremental_streaming_encodec, stream_artifact_selection, EncodeArtifacts,
    PreparedEncodeProgram, PreparedRequestEnvelope, ProgressSink, StreamArtifactSelection,
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

    fn remote_worker_config(&self) -> RemoteWorkerConfig {
        let mut config = RemoteWorkerConfig::new(
            self.config.upload_response_worker_id.clone(),
            PipelineSpec {
                source: SourceLane::Request,
                sink: SinkLane::Response,
            },
        );
        config.heartbeat_stage = "response".to_string();
        config.max_inflight = self.config.upload_response_max_inflight;
        config.poll_interval =
            Duration::from_millis(self.config.upload_response_worker_poll_ms.max(1));
        config.discovery_interval =
            Duration::from_millis(self.config.upload_response_discovery_interval_ms.max(1));
        config.ingress_urls = self.config.upload_response_ingress_urls.clone();
        config.discovery_dns = self.config.upload_response_discovery_dns.clone();
        config
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
        run_remote_worker_loop(client, self.remote_worker_config(), self).await;
    }

    async fn process_remote_job(&self, job: RemoteJob) -> Result<()> {
        if let Err(error) = self.process_remote_stream(&job).await {
            error!(
                origin = %job.origin,
                stream_id = job.stream_id,
                error = %error,
                "remote cached encode failed"
            );
            let response = error_response(classify_error(&error), error.to_string());
            job.client()
                .write_handler_response(&job.origin, job.stream_id, response)
                .await?;
        }
        Ok(())
    }

    async fn process_remote_stream(&self, job: &RemoteJob) -> Result<()> {
        let stream_id = job.stream_id;
        let request = job
            .request()
            .await?
            .ok_or_else(|| anyhow::anyhow!("request headers missing for stream {stream_id}"))?;
        let client = job.client();
        let origin = job.origin.as_str();

        if is_streaming_request(&request) {
            let mut writer = JsonLineResponseWriter::remote(
                client.clone(),
                origin.to_string(),
                stream_id,
                client.slot_bytes(),
            );
            return self.stream_remote_upload(job, request, &mut writer).await;
        }

        let body = self.read_remote_request_body(job).await?;
        let mut progress: Option<&mut (dyn ProgressSink + Send)> = None;
        let artifacts =
            if let Some(prepared) = prepared_program_from_internal_request(&request, &body)? {
                process_prepared_program(&self.config, prepared, &mut progress).await?
            } else {
                process_request(&self.config, &request, body, &mut progress).await?
            };
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
        job: &RemoteJob,
        request: Request<()>,
        writer: &mut JsonLineResponseWriter,
    ) -> Result<()> {
        let origin = job.origin.as_str();
        let stream_id = job.stream_id;
        let request_id = Uuid::new_v4().to_string();
        let artifact_selection = stream_artifact_selection(&request)?;
        info!(
            %origin,
            stream_id,
            request_id = %request_id,
            artifact = %artifact_selection.as_str(),
            "starting streaming encode request"
        );

        writer
            .send_value(json!({
                "type": "Metadata",
                "request_id": request_id.as_str(),
                "path": request.uri().path(),
                "artifact": artifact_selection.as_str(),
            }))
            .await?;

        let artifacts = if let Some(prepared) =
            prepared_request_envelope_from_internal_request(&request)?
        {
            self.process_streaming_prepared_remote_upload(
                job,
                request_id.as_str(),
                artifact_selection,
                prepared,
                writer,
            )
            .await?
        } else {
            let body = self
                .read_remote_streaming_request_body(job, writer, &request_id)
                .await?;
            info!(
                %origin,
                stream_id,
                request_id = %request_id,
                body_bytes = body.len(),
                "received streamed upload body"
            );
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
            self.send_stream_artifacts(writer, request_id.as_str(), artifact_selection, &artifacts)
                .await?;
            artifacts
        };
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
        info!(
            %origin,
            stream_id,
            request_id = %request_id,
            duration = artifacts.duration_seconds,
            ecdc_bytes = artifacts.ecdc.len(),
            png_bytes = artifacts.png.len(),
            "finished streaming encode request"
        );
        writer.finish().await
    }

    async fn read_remote_streaming_request_body(
        &self,
        job: &RemoteJob,
        writer: &mut JsonLineResponseWriter,
        request_id: &str,
    ) -> Result<Bytes> {
        let mut reader = job.source_reader_from(
            1,
            Duration::from_millis(self.config.upload_response_worker_poll_ms.max(1)),
        );
        let mut buffer = Vec::new();
        let mut last_reported_bytes = 0usize;
        let origin = job.origin.as_str();
        let stream_id = job.stream_id;

        while let Some(frame) = reader.next_frame().await? {
            match frame {
                SourceFrame::Body(bytes) => {
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
                SourceFrame::Control(RequestControl::Finalize) => {
                    writer
                        .send_value(json!({
                            "type": "UploadProgress",
                            "request_id": request_id,
                            "bytes_received": buffer.len(),
                            "finalize": true,
                        }))
                        .await?;
                }
                SourceFrame::Control(RequestControl::KeepAlive) => {}
                SourceFrame::End => break,
                SourceFrame::RequestHeaders(_) | SourceFrame::StageHead(_) => {}
            }
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
        debug!(
            %origin,
            stream_id,
            request_id,
            bytes_received = buffer.len(),
            "completed reading remote streaming request body"
        );

        Ok(Bytes::from(buffer))
    }

    async fn read_remote_request_body(&self, job: &RemoteJob) -> Result<Bytes> {
        let mut reader = job.source_reader_from(
            1,
            Duration::from_millis(self.config.upload_response_worker_poll_ms.max(1)),
        );
        let mut buffer = Vec::new();

        while let Some(frame) = reader.next_frame().await? {
            match frame {
                SourceFrame::Body(bytes) => buffer.extend_from_slice(&bytes),
                SourceFrame::Control(_) => {}
                SourceFrame::End => break,
                SourceFrame::RequestHeaders(_) | SourceFrame::StageHead(_) => {}
            }
        }

        Ok(Bytes::from(buffer))
    }

    async fn process_streaming_prepared_remote_upload(
        &self,
        job: &RemoteJob,
        request_id: &str,
        selection: StreamArtifactSelection,
        prepared: PreparedRequestEnvelope,
        writer: &mut JsonLineResponseWriter,
    ) -> Result<EncodeArtifacts> {
        let origin = job.origin.as_str();
        let stream_id = job.stream_id;
        writer
            .send_value(json!({
                "type": "Progress",
                "status": "loading",
                "msg": "Prepared canonical stereo PCM",
            }))
            .await?;
        writer
            .send_value(json!({
                "type": "Progress",
                "status": "encoding",
                "msg": "Encoding with EnCodec...",
                "bandwidthKbps": prepared.quality.bandwidth_kbps(),
            }))
            .await?;

        let mut handle = start_incremental_streaming_encodec(&self.config, &prepared);
        let mut reader = job.source_reader_from(
            1,
            Duration::from_millis(self.config.upload_response_worker_poll_ms.max(1)),
        );
        let mut last_reported_bytes = 0usize;
        let mut bytes_received = 0usize;
        let mut digest = Sha256::new();
        let mut saw_body = false;
        info!(
            %origin,
            stream_id,
            request_id,
            quality = %prepared.quality.as_str(),
            "encoding streamed prepared PCM upload"
        );

        if selection.includes_ecdc() {
            send_artifact_start(writer, request_id, "ecdc", "application/octet-stream", None)
                .await?;
        }

        while let Some(frame) = reader.next_frame().await? {
            match frame {
                SourceFrame::Body(bytes) => {
                    if bytes.is_empty() {
                        continue;
                    }
                    saw_body = true;
                    bytes_received += bytes.len();
                    digest.update(&bytes);
                    handle.send_pcm_chunk(bytes)?;
                    if bytes_received.saturating_sub(last_reported_bytes)
                        >= STREAM_UPLOAD_PROGRESS_BYTES
                    {
                        last_reported_bytes = bytes_received;
                        writer
                            .send_value(json!({
                                "type": "UploadProgress",
                                "request_id": request_id,
                                "bytes_received": bytes_received,
                            }))
                            .await?;
                    }
                }
                SourceFrame::Control(RequestControl::Finalize) => {
                    writer
                        .send_value(json!({
                            "type": "UploadProgress",
                            "request_id": request_id,
                            "bytes_received": bytes_received,
                            "finalize": true,
                        }))
                        .await?;
                }
                SourceFrame::Control(RequestControl::KeepAlive) => {}
                SourceFrame::End => break,
                SourceFrame::RequestHeaders(_) | SourceFrame::StageHead(_) => {}
            }
        }

        anyhow::ensure!(saw_body, "request body did not include audio bytes");
        handle.finish_input()?;

        writer
            .send_value(json!({
                "type": "UploadComplete",
                "request_id": request_id,
                "bytes_received": bytes_received,
            }))
            .await?;

        let duration_seconds = prepared.duration_seconds.unwrap_or_else(|| {
            pcm_duration_seconds(
                bytes_received,
                prepared.descriptor.sample_rate,
                prepared.descriptor.channels,
                prepared.descriptor.bits_per_sample,
            )
        });
        let audio_key = prepared.audio_key.clone().unwrap_or_else(|| {
            finalize_prepared_audio_key_digest(
                digest,
                &prepared.descriptor,
                prepared.quality,
                prepared.track_count,
                prepared.gap_seconds,
                prepared.total_gap_seconds,
            )
        });

        writer
            .send_value(json!({
                "type": "Request",
                "request_id": request_id,
                "track_count": prepared.track_count,
                "quality": prepared.quality.as_str(),
                "gap_seconds": prepared.gap_seconds,
                "audio_key": audio_key.as_str(),
            }))
            .await?;

        let mut artifact_chunks = 0usize;
        let mut pending_ecdc = Vec::new();
        while let Some(chunk) = handle.chunks.recv().await {
            if !selection.includes_ecdc() {
                continue;
            }
            pending_ecdc.extend_from_slice(&chunk);
            while pending_ecdc.len() >= STREAM_ARTIFACT_CHUNK_BYTES {
                let remainder = pending_ecdc.split_off(STREAM_ARTIFACT_CHUNK_BYTES);
                artifact_chunks += 1;
                send_artifact_chunk(
                    writer,
                    request_id,
                    "ecdc",
                    artifact_chunks - 1,
                    &pending_ecdc,
                )
                .await?;
                pending_ecdc = remainder;
            }
        }

        let ecdc = handle.finish().await?;
        info!(
            request_id,
            ecdc_bytes = ecdc.len(),
            artifact_chunks,
            duration = duration_seconds,
            "finished incremental streaming ECDC encode"
        );

        if selection.includes_ecdc() {
            if !pending_ecdc.is_empty() {
                artifact_chunks += 1;
                send_artifact_chunk(
                    writer,
                    request_id,
                    "ecdc",
                    artifact_chunks - 1,
                    &pending_ecdc,
                )
                .await?;
            }
            send_artifact_end(
                writer,
                request_id,
                "ecdc",
                artifact_chunks,
                Some(ecdc.len()),
            )
            .await?;
        }

        writer
            .send_value(json!({
                "type": "Progress",
                "status": "packing",
                "msg": "Packing bytes to RGB...",
            }))
            .await?;

        let prepared_program = PreparedEncodeProgram {
            pcm_bytes: Vec::new(),
            sample_rate: prepared.descriptor.sample_rate,
            channels: prepared.descriptor.channels,
            bits_per_sample: prepared.descriptor.bits_per_sample,
            duration_seconds,
            quality: prepared.quality,
            audio_key,
            track_count: prepared.track_count,
            gap_seconds: prepared.gap_seconds,
            total_gap_seconds: prepared.total_gap_seconds,
        };
        let artifacts = build_artifacts_from_ecdc(&self.config, &prepared_program, ecdc)?;
        if selection.includes_png() {
            send_artifact_chunks(writer, request_id, "png", "image/png", &artifacts.png).await?;
        }
        Ok(artifacts)
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
    send_artifact_start(writer, request_id, name, content_type, Some(bytes.len())).await?;

    let mut chunks = 0usize;
    for (index, chunk) in bytes.chunks(STREAM_ARTIFACT_CHUNK_BYTES).enumerate() {
        chunks = index + 1;
        send_artifact_chunk(writer, request_id, name, index, chunk).await?;
    }

    send_artifact_end(writer, request_id, name, chunks, Some(bytes.len())).await?;

    Ok(())
}

async fn send_artifact_start(
    writer: &mut JsonLineResponseWriter,
    request_id: &str,
    name: &str,
    content_type: &str,
    size_bytes: Option<usize>,
) -> Result<()> {
    let mut payload = serde_json::Map::new();
    payload.insert("type".into(), Value::String("ArtifactStart".into()));
    payload.insert("request_id".into(), Value::String(request_id.into()));
    payload.insert("name".into(), Value::String(name.into()));
    payload.insert("content_type".into(), Value::String(content_type.into()));
    payload.insert("encoding".into(), Value::String("base64".into()));
    if let Some(size_bytes) = size_bytes {
        payload.insert("size_bytes".into(), json!(size_bytes));
    }
    writer.send_value(Value::Object(payload)).await
}

async fn send_artifact_chunk(
    writer: &mut JsonLineResponseWriter,
    request_id: &str,
    name: &str,
    index: usize,
    bytes: &[u8],
) -> Result<()> {
    writer
        .send_value(json!({
            "type": "ArtifactChunk",
            "request_id": request_id,
            "name": name,
            "index": index,
            "data": base64::engine::general_purpose::STANDARD.encode(bytes),
        }))
        .await
}

async fn send_artifact_end(
    writer: &mut JsonLineResponseWriter,
    request_id: &str,
    name: &str,
    chunks: usize,
    size_bytes: Option<usize>,
) -> Result<()> {
    let mut payload = serde_json::Map::new();
    payload.insert("type".into(), Value::String("ArtifactEnd".into()));
    payload.insert("request_id".into(), Value::String(request_id.into()));
    payload.insert("name".into(), Value::String(name.into()));
    payload.insert("chunks".into(), json!(chunks));
    if let Some(size_bytes) = size_bytes {
        payload.insert("size_bytes".into(), json!(size_bytes));
    }
    writer.send_value(Value::Object(payload)).await
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

#[async_trait]
impl RemoteJobProcessor for WorkerState {
    async fn process(&self, job: RemoteJob) -> Result<()> {
        self.process_remote_job(job).await
    }
}
