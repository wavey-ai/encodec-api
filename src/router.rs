use crate::config::AppConfig;
use crate::ingress::{EncodeIngress, EncodeIngressWebSocketHandler};
use async_trait::async_trait;
use bytes::Bytes;
use http::{Method, Request, StatusCode};
use std::sync::Arc;
use upload_response::UploadResponseRouter;
use web_service::{
    BodyStream, HandlerResponse, HandlerResult, Router, ServerError, StreamWriter,
    WebSocketHandler, WebTransportHandler,
};

#[derive(Clone)]
pub struct AppRouter {
    config: AppConfig,
    upload: Option<Arc<UploadResponseRouter>>,
    encode: Option<Arc<EncodeIngress>>,
    encode_ws: Option<Arc<EncodeIngressWebSocketHandler>>,
}

impl AppRouter {
    pub fn new(
        config: AppConfig,
        upload: Option<Arc<UploadResponseRouter>>,
        encode: Option<Arc<EncodeIngress>>,
        encode_ws: Option<Arc<EncodeIngressWebSocketHandler>>,
    ) -> Self {
        Self {
            config,
            upload,
            encode,
            encode_ws,
        }
    }

    fn json_response(body: String) -> HandlerResponse {
        HandlerResponse {
            status: StatusCode::OK,
            body: Some(Bytes::from(body)),
            content_type: Some("application/json".into()),
            headers: vec![("cache-control".into(), "no-store".into())],
            etag: None,
        }
    }

    fn no_content() -> HandlerResponse {
        HandlerResponse {
            status: StatusCode::NO_CONTENT,
            body: None,
            content_type: None,
            headers: vec![("cache-control".into(), "no-store".into())],
            etag: None,
        }
    }

    fn not_found() -> HandlerResponse {
        HandlerResponse {
            status: StatusCode::NOT_FOUND,
            body: Some(Bytes::from("{\"error\":\"not found\"}")),
            content_type: Some("application/json".into()),
            headers: vec![("cache-control".into(), "no-store".into())],
            etag: None,
        }
    }

    fn is_upload_path(path: &str) -> bool {
        path == "/_upload_response/streams" || path.starts_with("/_upload_response/streams/")
    }

    fn is_encode_path(path: &str) -> bool {
        matches!(path, "/encode" | "/encode/ecdc" | "/encode/stream")
    }
}

#[async_trait]
impl Router for AppRouter {
    async fn route(&self, req: Request<()>) -> HandlerResult<HandlerResponse> {
        match (req.method(), req.uri().path()) {
            (&Method::GET, "/") | (&Method::GET, "/health") | (&Method::GET, "/healthz") => {
                Ok(Self::json_response(
                    "{\"status\":\"ok\",\"service\":\"encodec-api\"}".into(),
                ))
            }
            (&Method::GET, "/status") => Ok(Self::json_response(format!(
                "{{\"status\":\"ok\",\"service\":\"encodec-api\",\"role\":\"{:?}\",\"backend\":\"{}\",\"execution_target\":\"{}\"}}",
                self.config.role,
                self.config.encodec_backend_label(),
                self.config.execution_target_label()
            ))),
            (&Method::GET, "/info") => Ok(Self::json_response(format!(
                "{{\"service\":\"encodec-api\",\"paths\":[\"/encode\",\"/encode/ecdc\",\"/encode/stream\"],\"transport\":\"upload-response\",\"streaming\":\"ndjson\",\"scale\":\"cpu-ingress/gpu-worker\",\"backend\":\"{}\",\"execution_target\":\"{}\"}}",
                self.config.encodec_backend_label(),
                self.config.execution_target_label()
            ))),
            (&Method::GET, "/ha/live") | (&Method::HEAD, "/ha/live") => Ok(HandlerResponse {
                status: StatusCode::OK,
                body: (req.method() == Method::GET).then(|| Bytes::from("OK")),
                content_type: (req.method() == Method::GET).then_some("text/plain".into()),
                headers: vec![("cache-control".into(), "no-store".into())],
                etag: None,
            }),
            (&Method::GET, "/ha/available") | (&Method::HEAD, "/ha/available") => {
                Ok(Self::no_content())
            }
            (&Method::GET, "/ha/version") | (&Method::HEAD, "/ha/version") => Ok(
                Self::json_response(format!(
                    "{{\"sha\":\"{}\",\"role\":\"{:?}\"}}",
                    option_env!("GIT_SHA").unwrap_or("dev"),
                    self.config.role
                )),
            ),
            (&Method::OPTIONS, _) if Self::is_encode_path(req.uri().path()) && self.encode.is_some() => {
                Ok(Self::no_content())
            }
            _ if Self::is_upload_path(req.uri().path()) => {
                if let Some(upload) = &self.upload {
                    upload.route(req).await
                } else {
                    Ok(Self::not_found())
                }
            }
            _ => Ok(Self::not_found()),
        }
    }

    async fn route_body(
        &self,
        req: Request<()>,
        body: BodyStream,
    ) -> HandlerResult<HandlerResponse> {
        if Self::is_encode_path(req.uri().path()) {
            if let Some(encode) = &self.encode {
                Ok(encode.handle_encode(req, body).await)
            } else {
                Ok(Self::not_found())
            }
        } else if Self::is_upload_path(req.uri().path()) {
            if let Some(upload) = &self.upload {
                upload.route_body(req, body).await
            } else {
                Ok(Self::not_found())
            }
        } else {
            Ok(Self::not_found())
        }
    }

    fn has_body_handler(&self, path: &str) -> bool {
        (self.encode.is_some() && Self::is_encode_path(path))
            || (self.upload.is_some() && Self::is_upload_path(path))
    }

    fn has_body_stream_handler(&self, path: &str) -> bool {
        self.encode.is_some() && path == "/encode/stream"
    }

    fn is_streaming(&self, path: &str) -> bool {
        path == "/encode/stream"
    }

    async fn route_body_stream(
        &self,
        req: Request<()>,
        body: BodyStream,
        stream_writer: Box<dyn StreamWriter>,
    ) -> HandlerResult<()> {
        if req.uri().path() == "/encode/stream" {
            if let Some(encode) = &self.encode {
                encode.handle_encode_stream(req, body, stream_writer).await
            } else {
                Err(ServerError::Config("encode ingress not configured".into()))
            }
        } else {
            Err(ServerError::Config(
                "streaming request/response is not supported for this route".into(),
            ))
        }
    }

    async fn route_stream(
        &self,
        _req: Request<()>,
        _stream_writer: Box<dyn StreamWriter>,
    ) -> HandlerResult<()> {
        Err(ServerError::Config(
            "stream-only routes are not supported".into(),
        ))
    }

    fn webtransport_handler(&self) -> Option<&dyn WebTransportHandler> {
        None
    }

    fn websocket_handler(&self, _path: &str) -> Option<&dyn WebSocketHandler> {
        if let Some(handler) = &self.encode_ws {
            if handler.can_handle(_path) {
                return Some(handler.as_ref());
            }
        }
        None
    }
}
