use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{stream::SplitSink, SinkExt, StreamExt};
use http::{header::CONTENT_TYPE, Request, StatusCode};
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tokio::time::{interval, timeout, Duration};
use tokio_tungstenite::{tungstenite::Message, WebSocketStream};
use tracing::{debug, info, warn};
use upload_response::{
    response_content_type, CachedIngress, IngressProxyConfig, TailSlot, UploadResponseService,
};
use web_service::{
    BodyStream, HandlerResponse, HandlerResult, ServerError, StreamWriter, WebSocketHandler,
};

use crate::config::AppConfig;
use crate::encode::{
    apply_prepared_request_headers, apply_prepared_request_metadata_headers, client_error,
    parse_request, prepare_request_program, program_pcm_descriptor, request_encode_options,
    request_filename, PreparedEncodeProgram, PreparedRequestMetadata,
};
use soundkit_decoder::{DecodeError, DecodeOptions, DecodePipeline, DecodePipelineHandle};

#[derive(Clone)]
pub struct EncodeIngress {
    config: AppConfig,
    cached: CachedIngress,
    service: Arc<UploadResponseService>,
}

#[derive(Clone)]
pub struct EncodeIngressWebSocketHandler {
    ingress: Arc<EncodeIngress>,
}

#[derive(Debug, Deserialize)]
struct WsClientEvent {
    #[serde(rename = "type")]
    event_type: String,
}

impl EncodeIngress {
    pub fn new(config: AppConfig, service: Arc<UploadResponseService>) -> Self {
        Self {
            config: config.clone(),
            cached: CachedIngress::new(
                service.clone(),
                IngressProxyConfig {
                    response_timeout_ms: config.upload_response_timeout_ms,
                    watch_poll_ms: config.upload_response_watch_poll_ms,
                },
            ),
            service,
        }
    }

    pub async fn handle_encode(&self, req: Request<()>, body: BodyStream) -> HandlerResponse {
        match self.handle_encode_inner(req, body).await {
            Ok(response) => response,
            Err(error) => error_response(classify_error(&error), error.to_string()),
        }
    }

    pub async fn handle_encode_stream(
        &self,
        req: Request<()>,
        body: BodyStream,
        stream_writer: Box<dyn StreamWriter>,
    ) -> HandlerResult<()> {
        reject_json_requests(&req).map_err(anyhow_to_server_error)?;
        let guard = self
            .cached
            .open_streaming_request()
            .await
            .map_err(anyhow_to_server_error)?;
        let stream_id = guard.stream_id();
        info!(
            stream_id,
            path = %req.uri().path(),
            direct_stream_decode = request_supports_stream_decode(&req),
            "accepted streaming encode request"
        );

        if let Err(error) = self.cache_request_body(stream_id, &req, body, true).await {
            warn!(stream_id, error = %error, "failed to cache streaming request body");
            guard.close().await;
            return Err(anyhow_to_server_error(error));
        }

        let proxy_result = self
            .cached
            .proxy_streaming_response(stream_id, stream_writer)
            .await;
        guard.close().await;
        proxy_result
    }

    pub async fn handle_encode_websocket(
        &self,
        req: Request<()>,
        stream: WebSocketStream<TokioIo<Upgraded>>,
    ) -> HandlerResult<()> {
        reject_json_requests(&req).map_err(anyhow_to_server_error)?;
        let (sink, mut source) = stream.split();
        let mut body = Vec::new();

        while let Some(frame) = source.next().await {
            match frame {
                Ok(Message::Binary(bytes)) => body.extend_from_slice(&bytes),
                Ok(Message::Text(text)) => {
                    let event =
                        serde_json::from_str::<WsClientEvent>(&text).unwrap_or(WsClientEvent {
                            event_type: String::new(),
                        });
                    match event.event_type.as_str() {
                        "KeepAlive" => {}
                        "Finalize" | "CloseStream" => break,
                        _ => {}
                    }
                }
                Ok(Message::Ping(_)) => {}
                Ok(Message::Pong(_)) => {}
                Ok(Message::Close(_)) => break,
                Ok(Message::Frame(_)) => {}
                Err(error) => {
                    return Err(ServerError::Config(format!(
                        "websocket receive failed: {error}"
                    )));
                }
            }
        }

        let prepared = self
            .prepare_request_bytes(&req, Bytes::from(body))
            .await
            .map_err(anyhow_to_server_error)?;

        let upload_stream = self
            .cached
            .open_streaming_request()
            .await
            .map_err(anyhow_to_server_error)?;
        let stream_id = upload_stream.stream_id();

        if let Err(error) = self
            .cache_prepared_request(stream_id, &req, &prepared, true)
            .await
        {
            upload_stream.close().await;
            return Err(anyhow_to_server_error(error));
        }
        let proxy_result = self.proxy_websocket_response(stream_id, sink).await;
        upload_stream.close().await;
        proxy_result
    }

    async fn handle_encode_inner(
        &self,
        req: Request<()>,
        body: BodyStream,
    ) -> Result<HandlerResponse> {
        reject_json_requests(&req)?;
        let mut guard = self.cached.open_buffered_request().await?;
        let stream_id = guard.stream_id();

        let result = async {
            self.cache_request_body(stream_id, &req, body, false)
                .await?;

            let rx = guard
                .take_response_receiver()
                .ok_or_else(|| anyhow::anyhow!("response receiver missing for buffered request"))?;
            self.cached.await_response(stream_id, rx).await
        }
        .await;

        guard.close().await;
        result
    }

    async fn cache_request_body(
        &self,
        stream_id: u64,
        req: &Request<()>,
        body: BodyStream,
        streaming: bool,
    ) -> Result<()> {
        debug!(
            stream_id,
            path = %req.uri().path(),
            direct_stream_decode = request_supports_stream_decode(req),
            streaming,
            "caching request body"
        );
        if request_supports_stream_decode(req) {
            self.cache_streamed_direct_request(stream_id, req, body, streaming)
                .await
        } else {
            let prepared = self.prepare_request_body(req, body).await?;
            self.cache_prepared_request(stream_id, req, &prepared, streaming)
                .await
        }
    }

    async fn write_cached_request_headers(
        &self,
        stream_id: u64,
        req: &Request<()>,
        prepared: &PreparedEncodeProgram,
        streaming: bool,
    ) -> Result<()> {
        self.cached
            .write_request_headers_with(stream_id, req, |headers| {
                apply_prepared_request_headers(headers, prepared, streaming)
            })
            .await
    }

    async fn cache_prepared_request(
        &self,
        stream_id: u64,
        req: &Request<()>,
        prepared: &PreparedEncodeProgram,
        streaming: bool,
    ) -> Result<()> {
        self.write_cached_request_headers(stream_id, req, prepared, streaming)
            .await?;
        self.cached
            .append_request_body_sliced(
                stream_id,
                &prepared.pcm_bytes,
                self.config.upload_response_config().slot_bytes().max(1),
            )
            .await?;
        self.cached.end_request(stream_id).await
    }

    async fn cache_streamed_direct_request(
        &self,
        stream_id: u64,
        req: &Request<()>,
        mut body: BodyStream,
        streaming: bool,
    ) -> Result<()> {
        let (quality, gap_seconds) = request_encode_options(req)?;
        let descriptor = program_pcm_descriptor();
        self.cached
            .write_request_headers_with(stream_id, req, |headers| {
                apply_prepared_request_metadata_headers(
                    headers,
                    PreparedRequestMetadata {
                        sample_rate: descriptor.sample_rate,
                        channels: descriptor.channels,
                        bits_per_sample: descriptor.bits_per_sample,
                        quality,
                        track_count: 1,
                        gap_seconds,
                        total_gap_seconds: 0.0,
                        duration_seconds: None,
                        audio_key: None,
                        streaming,
                    },
                )
            })
            .await?;
        info!(
            stream_id,
            quality = %quality.as_str(),
            gap_seconds,
            streaming,
            "starting direct streamed decode to canonical PCM"
        );

        let mut pipeline = DecodePipeline::spawn_with_buffers_and_options(
            1024,
            1024,
            DecodeOptions {
                output_bits_per_sample: Some(descriptor.bits_per_sample),
                output_sample_rate: Some(descriptor.sample_rate),
                output_channels: Some(descriptor.channels),
            },
        );
        let mut decoded_body_bytes = 0usize;
        let mut upload_bytes = 0usize;

        while let Some(next) = body.next().await {
            let chunk =
                next.map_err(|error| anyhow::anyhow!("failed to read request body: {error}"))?;
            if chunk.is_empty() {
                continue;
            }
            upload_bytes += chunk.len();
            self.send_decoder_chunk(stream_id, &mut pipeline, chunk, &mut decoded_body_bytes)
                .await?;
        }

        self.send_decoder_chunk(
            stream_id,
            &mut pipeline,
            Bytes::new(),
            &mut decoded_body_bytes,
        )
        .await?;
        self.flush_decoder_outputs(stream_id, &mut pipeline, &mut decoded_body_bytes)
            .await?;

        anyhow::ensure!(
            decoded_body_bytes > 0,
            "request body did not include decodable audio"
        );
        info!(
            stream_id,
            upload_bytes, decoded_body_bytes, "finished direct streamed decode to canonical PCM"
        );

        self.cached.end_request(stream_id).await
    }

    async fn prepare_request_body(
        &self,
        req: &Request<()>,
        body: BodyStream,
    ) -> Result<PreparedEncodeProgram> {
        let body = collect_body(body).await?;
        self.prepare_request_bytes(req, body).await
    }

    async fn prepare_request_bytes(
        &self,
        req: &Request<()>,
        body: Bytes,
    ) -> Result<PreparedEncodeProgram> {
        let parsed = parse_request(req, body).await?;
        prepare_request_program(parsed)
    }

    async fn proxy_websocket_response(
        &self,
        stream_id: u64,
        mut sink: SplitSink<WebSocketStream<TokioIo<Upgraded>>, Message>,
    ) -> HandlerResult<()> {
        let timeout_duration = Duration::from_millis(self.config.upload_response_timeout_ms);
        timeout(timeout_duration, async {
            let mut poll = interval(Duration::from_millis(
                self.config.upload_response_watch_poll_ms.max(1),
            ));
            let mut last_slot = 0usize;
            let mut headers_seen = false;
            let mut stream_json_lines = false;
            let mut line_buffer = Vec::new();

            loop {
                poll.tick().await;

                if !headers_seen {
                    if let Some(headers) = self.service.get_response_headers(stream_id).await {
                        stream_json_lines = response_content_type(&headers)
                            .map(|value| value.contains("ndjson"))
                            .unwrap_or(false);
                        headers_seen = true;
                        last_slot = 1;
                    } else {
                        continue;
                    }
                }

                let current_last = self.service.response_last(stream_id).unwrap_or(0);
                if current_last <= last_slot {
                    continue;
                }

                for slot_id in (last_slot + 1)..=current_last {
                    match self.service.tail_response(stream_id, slot_id).await {
                        Some(TailSlot::Body(bytes)) if stream_json_lines => {
                            line_buffer.extend_from_slice(&bytes);
                            while let Some(position) =
                                line_buffer.iter().position(|byte| *byte == b'\n')
                            {
                                let line = line_buffer.drain(..=position).collect::<Vec<u8>>();
                                let line = trim_newline(&line);
                                if line.is_empty() {
                                    continue;
                                }
                                sink.send(Message::Text(
                                    String::from_utf8_lossy(line).into_owned().into(),
                                ))
                                .await
                                .map_err(|error| ServerError::Handler(Box::new(error)))?;
                            }
                        }
                        Some(TailSlot::Body(bytes)) => {
                            send_ws_body_chunk(&mut sink, bytes).await?;
                        }
                        Some(TailSlot::End) => {
                            if stream_json_lines && !line_buffer.is_empty() {
                                let line = trim_newline(&line_buffer);
                                if !line.is_empty() {
                                    sink.send(Message::Text(
                                        String::from_utf8_lossy(line).into_owned().into(),
                                    ))
                                    .await
                                    .map_err(|error| ServerError::Handler(Box::new(error)))?;
                                }
                            }
                            let _ = sink.close().await;
                            return Ok(());
                        }
                        _ => {}
                    }
                }

                last_slot = current_last;
            }
        })
        .await
        .map_err(|_| ServerError::Config("response timeout".into()))?
    }

    async fn send_decoder_chunk(
        &self,
        stream_id: u64,
        pipeline: &mut DecodePipelineHandle,
        chunk: Bytes,
        decoded_body_bytes: &mut usize,
    ) -> Result<()> {
        loop {
            match pipeline.send(chunk.clone()) {
                Ok(()) => {
                    self.drain_decoder_outputs(stream_id, pipeline, decoded_body_bytes)
                        .await?;
                    return Ok(());
                }
                Err(DecodeError::InputBufferFull) => {
                    self.drain_decoder_outputs(stream_id, pipeline, decoded_body_bytes)
                        .await?;
                    tokio::task::yield_now().await;
                }
                Err(error) => {
                    return Err(anyhow::anyhow!("decoder input failed: {error}"));
                }
            }
        }
    }

    async fn drain_decoder_outputs(
        &self,
        stream_id: u64,
        pipeline: &mut DecodePipelineHandle,
        decoded_body_bytes: &mut usize,
    ) -> Result<()> {
        let chunk_size = self.config.upload_response_config().slot_bytes().max(1);
        while let Some(output) = pipeline.try_recv() {
            let audio = match output {
                Ok(audio) => audio,
                Err(error) => return Err(anyhow::anyhow!("audio decode failed: {error}")),
            };
            *decoded_body_bytes += audio.data().len();
            self.cached
                .append_request_body_sliced(stream_id, audio.data(), chunk_size)
                .await?;
        }
        debug!(
            stream_id,
            decoded_body_bytes = *decoded_body_bytes,
            "flushed decoder outputs"
        );
        Ok(())
    }

    async fn flush_decoder_outputs(
        &self,
        stream_id: u64,
        pipeline: &mut DecodePipelineHandle,
        decoded_body_bytes: &mut usize,
    ) -> Result<()> {
        loop {
            self.drain_decoder_outputs(stream_id, pipeline, decoded_body_bytes)
                .await?;
            match tokio::task::block_in_place(|| pipeline.recv()) {
                Some(output) => {
                    let audio = match output {
                        Ok(audio) => audio,
                        Err(error) => {
                            return Err(anyhow::anyhow!("audio decode failed: {error}"));
                        }
                    };
                    *decoded_body_bytes += audio.data().len();
                    self.cached
                        .append_request_body_sliced(
                            stream_id,
                            audio.data(),
                            self.config.upload_response_config().slot_bytes().max(1),
                        )
                        .await?;
                }
                None => return Ok(()),
            }
        }
    }
}

impl EncodeIngressWebSocketHandler {
    pub fn new(ingress: Arc<EncodeIngress>) -> Self {
        Self { ingress }
    }
}

#[async_trait]
impl WebSocketHandler for EncodeIngressWebSocketHandler {
    async fn handle_websocket(
        &self,
        req: Request<()>,
        stream: WebSocketStream<TokioIo<Upgraded>>,
    ) -> HandlerResult<()> {
        self.ingress.handle_encode_websocket(req, stream).await
    }

    fn can_handle(&self, path: &str) -> bool {
        path == "/encode/stream"
    }
}

async fn send_ws_body_chunk(
    sink: &mut SplitSink<WebSocketStream<TokioIo<Upgraded>>, Message>,
    bytes: Bytes,
) -> HandlerResult<()> {
    if let Ok(text) = std::str::from_utf8(&bytes) {
        sink.send(Message::Text(text.to_string().into()))
            .await
            .map_err(|error| ServerError::Handler(Box::new(error)))
    } else {
        sink.send(Message::Binary(bytes))
            .await
            .map_err(|error| ServerError::Handler(Box::new(error)))
    }
}

fn trim_newline(bytes: &[u8]) -> &[u8] {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    bytes.strip_suffix(b"\r").unwrap_or(bytes)
}

fn reject_json_requests(req: &Request<()>) -> Result<()> {
    let is_json = req
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            let value = value.to_ascii_lowercase();
            value.starts_with("application/json") || value.ends_with("+json")
        })
        .unwrap_or(false);

    if is_json {
        return Err(client_error(
            "JSON request bodies are not supported; upload audio bytes instead",
        ));
    }

    Ok(())
}

fn request_supports_stream_decode(req: &Request<()>) -> bool {
    let content_type = req
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_ascii_lowercase());

    if content_type
        .as_deref()
        .map(|value| value.starts_with("multipart/"))
        .unwrap_or(false)
    {
        return false;
    }

    if content_type
        .as_deref()
        .map(is_streamable_audio_content_type)
        .unwrap_or(false)
    {
        return true;
    }

    let filename = request_filename(req);
    filename
        .rsplit_once('.')
        .map(|(_, extension)| is_streamable_audio_extension(extension))
        .unwrap_or(false)
}

fn is_streamable_audio_content_type(content_type: &str) -> bool {
    matches!(
        content_type,
        "audio/mpeg"
            | "audio/wav"
            | "audio/x-wav"
            | "audio/wave"
            | "audio/flac"
            | "audio/mp4"
            | "audio/x-m4a"
            | "audio/aac"
            | "audio/ogg"
            | "audio/opus"
            | "audio/webm"
            | "video/webm"
    )
}

fn is_streamable_audio_extension(extension: &str) -> bool {
    matches!(
        extension.trim().to_ascii_lowercase().as_str(),
        "mp3" | "wav" | "wave" | "flac" | "m4a" | "mp4" | "aac" | "ogg" | "oga" | "opus" | "webm"
    )
}

fn error_response(status: StatusCode, message: String) -> HandlerResponse {
    HandlerResponse {
        status,
        body: Some(Bytes::from(
            serde_json::to_vec(&json!({ "error": message }))
                .unwrap_or_else(|_| b"{\"error\":\"serialization failure\"}".to_vec()),
        )),
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

fn anyhow_to_server_error(error: anyhow::Error) -> ServerError {
    ServerError::Config(error.to_string())
}

async fn collect_body(mut body: BodyStream) -> Result<Bytes> {
    let mut buffer = Vec::new();
    while let Some(next) = body.next().await {
        let chunk =
            next.map_err(|error| anyhow::anyhow!("failed to read request body: {error}"))?;
        if chunk.is_empty() {
            continue;
        }
        buffer.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buffer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::Request;

    #[test]
    fn stream_decode_supports_known_direct_audio_types() {
        let wav = Request::builder()
            .uri("/encode")
            .header(CONTENT_TYPE, "audio/wav")
            .body(())
            .unwrap();
        assert!(request_supports_stream_decode(&wav));

        let mp3 = Request::builder()
            .uri("/encode")
            .header("x-filename", "clip.mp3")
            .body(())
            .unwrap();
        assert!(request_supports_stream_decode(&mp3));

        let webm = Request::builder()
            .uri("/encode")
            .header(CONTENT_TYPE, "video/webm")
            .body(())
            .unwrap();
        assert!(request_supports_stream_decode(&webm));
    }

    #[test]
    fn stream_decode_rejects_multipart_and_unknown_types() {
        let multipart = Request::builder()
            .uri("/encode")
            .header(CONTENT_TYPE, "multipart/form-data; boundary=abc123")
            .body(())
            .unwrap();
        assert!(!request_supports_stream_decode(&multipart));

        let unknown = Request::builder()
            .uri("/encode")
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(())
            .unwrap();
        assert!(!request_supports_stream_decode(&unknown));
    }
}
