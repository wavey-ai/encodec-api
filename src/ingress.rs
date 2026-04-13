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
use upload_response::{
    response_content_type, CachedIngress, IngressProxyConfig, TailSlot, UploadResponseService,
};
use web_service::{
    BodyStream, HandlerResponse, HandlerResult, ServerError, StreamWriter, WebSocketHandler,
};

use crate::config::AppConfig;
use crate::encode::{
    apply_prepared_request_headers, client_error, parse_request, prepare_request_program,
    PreparedEncodeProgram,
};

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
        let prepared = self
            .prepare_request_body(&req, body)
            .await
            .map_err(anyhow_to_server_error)?;

        let guard = self
            .cached
            .open_streaming_request()
            .await
            .map_err(anyhow_to_server_error)?;
        let stream_id = guard.stream_id();

        self.cache_prepared_request(stream_id, &req, &prepared, true)
            .await
            .map_err(anyhow_to_server_error)?;

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

        self.cache_prepared_request(stream_id, &req, &prepared, true)
            .await
            .map_err(anyhow_to_server_error)?;
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
        let prepared = self.prepare_request_body(&req, body).await?;

        let mut guard = self.cached.open_buffered_request().await?;
        let stream_id = guard.stream_id();

        let result = async {
            self.cache_prepared_request(stream_id, &req, &prepared, false)
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
