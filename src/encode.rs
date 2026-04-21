use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use av_api::cached_audio::{
    apply_cached_pcm_descriptor_headers, cached_pcm_descriptor_from_headers, CachedPcmDescriptor,
    CachedPcmFormat, PCM_FORMAT_HEADER,
};
use av_api::program_audio::{prepare_audio_program, UploadedAudioFile};
use bytes::Bytes;
use encodec_rs::ecdc::{
    encode_audio_to_ecdc_stream_with_options, encode_audio_to_ecdc_with_options,
};
use encodec_rs::onnx::{ExecutionTarget, OnnxFrameCodec, OnnxLmCodec};
use futures_util::stream;
use http::{
    header::{CONTENT_LENGTH, CONTENT_TYPE},
    HeaderMap, HeaderValue, Request,
};
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ExtendedColorType, ImageEncoder};
use multer::Multipart;
use ndarray::Array3;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::task::JoinHandle;

use crate::config::AppConfig;
use crate::protocol::{
    INTERNAL_AUDIO_KEY_HEADER, INTERNAL_DURATION_SECONDS_HEADER, INTERNAL_GAP_SECONDS_HEADER,
    INTERNAL_QUALITY_HEADER, INTERNAL_STREAMING_MODE_HEADER, INTERNAL_STREAMING_MODE_JSONL,
    INTERNAL_TOTAL_GAP_SECONDS_HEADER, INTERNAL_TRACK_COUNT_HEADER,
};

pub const DEFAULT_TRACK_GAP_SECONDS: f64 = 1.5;
pub const PNG_FORMAT: &str = "rgb-v2";
pub const MODEL_NAME: &str = "encodec_48khz";
pub const DEFAULT_STREAM_ARTIFACT: &str = "png";

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ClientError(pub String);

pub fn client_error(message: impl Into<String>) -> anyhow::Error {
    ClientError(message.into()).into()
}

pub fn is_client_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ClientError>().is_some()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeQuality {
    Standard,
    Hq,
}

impl EncodeQuality {
    pub fn parse(raw: Option<&str>) -> Result<Self> {
        match raw.unwrap_or("hq").trim().to_ascii_lowercase().as_str() {
            "standard" => Ok(Self::Standard),
            "hq" | "" => Ok(Self::Hq),
            other => Err(client_error(format!(
                "unsupported quality {other:?}. Use one of: standard, hq"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Hq => "hq",
        }
    }

    pub fn bandwidth_kbps(self) -> f32 {
        match self {
            Self::Standard => 6.0,
            Self::Hq => 12.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct UploadedFile {
    pub filename: String,
    pub content_type: Option<String>,
    pub bytes: Bytes,
}

#[derive(Debug, Clone)]
pub struct ParsedEncodeRequest {
    pub files: Vec<UploadedFile>,
    pub gap_seconds: f64,
    pub quality: EncodeQuality,
    pub audio_key: String,
}

#[derive(Debug, Clone)]
pub struct EncodeArtifacts {
    pub ecdc: Bytes,
    pub png: Bytes,
    pub png_side: usize,
    pub device_label: String,
    pub duration_seconds: f64,
    pub quality: EncodeQuality,
    pub audio_key: String,
    pub track_count: usize,
    pub gap_seconds: f64,
    pub total_gap_seconds: f64,
}

#[derive(Debug, Clone)]
pub struct PreparedEncodeProgram {
    pub pcm_bytes: Vec<u8>,
    pub sample_rate: u32,
    pub channels: u8,
    pub bits_per_sample: u8,
    pub duration_seconds: f64,
    pub quality: EncodeQuality,
    pub audio_key: String,
    pub track_count: usize,
    pub gap_seconds: f64,
    pub total_gap_seconds: f64,
}

pub struct StreamingEncodeHandle {
    pub chunks: UnboundedReceiver<Bytes>,
    join: JoinHandle<Result<Bytes>>,
}

struct OnnxEncodeRuntime {
    frame: OnnxFrameCodec,
    lm: OnnxLmCodec,
}

static ONNX_RUNTIME_CACHE: OnceLock<Mutex<HashMap<String, Arc<Mutex<OnnxEncodeRuntime>>>>> =
    OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamArtifactSelection {
    Png,
    Ecdc,
    Both,
}

impl StreamArtifactSelection {
    pub fn parse(raw: Option<&str>) -> Result<Self> {
        match raw
            .unwrap_or(DEFAULT_STREAM_ARTIFACT)
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "png" | "" => Ok(Self::Png),
            "ecdc" => Ok(Self::Ecdc),
            "both" | "all" => Ok(Self::Both),
            other => Err(client_error(format!(
                "unsupported artifact {other:?}. Use one of: png, ecdc, both"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Ecdc => "ecdc",
            Self::Both => "both",
        }
    }

    pub fn includes_png(self) -> bool {
        matches!(self, Self::Png | Self::Both)
    }

    pub fn includes_ecdc(self) -> bool {
        matches!(self, Self::Ecdc | Self::Both)
    }
}

#[async_trait]
pub trait ProgressSink: Send {
    async fn send(&mut self, payload: Value) -> Result<()>;
}

pub async fn process_request(
    config: &AppConfig,
    req: &Request<()>,
    body: Bytes,
    progress: &mut Option<&mut (dyn ProgressSink + Send)>,
) -> Result<EncodeArtifacts> {
    let parsed = parse_request(req, body).await?;
    let prepared = prepare_request_program(parsed)?;
    process_prepared_program(config, prepared, progress).await
}

pub async fn process_parsed_request(
    config: &AppConfig,
    parsed: ParsedEncodeRequest,
    progress: &mut Option<&mut (dyn ProgressSink + Send)>,
) -> Result<EncodeArtifacts> {
    let prepared = prepare_request_program(parsed)?;
    process_prepared_program(config, prepared, progress).await
}

pub async fn process_prepared_program(
    config: &AppConfig,
    prepared: PreparedEncodeProgram,
    progress: &mut Option<&mut (dyn ProgressSink + Send)>,
) -> Result<EncodeArtifacts> {
    emit_progress(
        progress,
        json!({
            "type": "Progress",
            "status": "loading",
            "msg": "Prepared canonical stereo PCM",
            "duration": round_one(prepared.duration_seconds),
        }),
    )
    .await?;

    emit_progress(
        progress,
        json!({
            "type": "Progress",
            "status": "encoding",
            "msg": "Encoding with EnCodec...",
            "duration": round_one(prepared.duration_seconds),
            "bandwidthKbps": prepared.quality.bandwidth_kbps(),
        }),
    )
    .await?;

    let ecdc = run_encodec(config, &prepared).await?;

    emit_progress(
        progress,
        json!({ "type": "Progress", "status": "packing", "msg": "Packing bytes to RGB..." }),
    )
    .await?;

    build_artifacts_from_ecdc(config, &prepared, ecdc)
}

pub fn apply_prepared_request_headers(
    headers: &mut HeaderMap,
    prepared: &PreparedEncodeProgram,
    streaming: bool,
) -> Result<()> {
    headers.remove(CONTENT_LENGTH);
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );

    apply_cached_pcm_descriptor_headers(
        headers,
        CachedPcmDescriptor::new(
            CachedPcmFormat::S16LeInterleaved,
            prepared.sample_rate,
            prepared.channels,
            prepared.bits_per_sample,
        ),
    )
    .map_err(anyhow::Error::msg)?;

    insert_header(
        headers,
        INTERNAL_QUALITY_HEADER,
        prepared.quality.as_str().to_string(),
    )?;
    insert_header(
        headers,
        INTERNAL_TRACK_COUNT_HEADER,
        prepared.track_count.to_string(),
    )?;
    insert_header(
        headers,
        INTERNAL_GAP_SECONDS_HEADER,
        prepared.gap_seconds.to_string(),
    )?;
    insert_header(
        headers,
        INTERNAL_TOTAL_GAP_SECONDS_HEADER,
        prepared.total_gap_seconds.to_string(),
    )?;
    insert_header(
        headers,
        INTERNAL_DURATION_SECONDS_HEADER,
        prepared.duration_seconds.to_string(),
    )?;
    insert_header(
        headers,
        INTERNAL_AUDIO_KEY_HEADER,
        prepared.audio_key.clone(),
    )?;

    if streaming {
        headers.insert(
            INTERNAL_STREAMING_MODE_HEADER,
            HeaderValue::from_static(INTERNAL_STREAMING_MODE_JSONL),
        );
    } else {
        headers.remove(INTERNAL_STREAMING_MODE_HEADER);
    }

    Ok(())
}

pub fn prepared_program_from_internal_request(
    request: &Request<()>,
    body: &[u8],
) -> Result<Option<PreparedEncodeProgram>> {
    if !request.headers().contains_key(PCM_FORMAT_HEADER) {
        return Ok(None);
    }

    let descriptor =
        cached_pcm_descriptor_from_headers(request.headers()).map_err(anyhow::Error::msg)?;
    let quality = EncodeQuality::parse(Some(required_header(
        request.headers(),
        INTERNAL_QUALITY_HEADER,
    )?))?;
    let track_count =
        parse_required_header::<usize>(request.headers(), INTERNAL_TRACK_COUNT_HEADER)?;
    let gap_seconds = parse_required_header::<f64>(request.headers(), INTERNAL_GAP_SECONDS_HEADER)?;
    let total_gap_seconds =
        parse_required_header::<f64>(request.headers(), INTERNAL_TOTAL_GAP_SECONDS_HEADER)?;
    let duration_seconds =
        parse_required_header::<f64>(request.headers(), INTERNAL_DURATION_SECONDS_HEADER)?;
    let audio_key = required_header(request.headers(), INTERNAL_AUDIO_KEY_HEADER)?.to_string();

    anyhow::ensure!(
        !body.is_empty(),
        "prepared internal request body did not include PCM bytes"
    );

    Ok(Some(PreparedEncodeProgram {
        pcm_bytes: body.to_vec(),
        sample_rate: descriptor.sample_rate,
        channels: descriptor.channels,
        bits_per_sample: descriptor.bits_per_sample,
        duration_seconds,
        quality,
        audio_key,
        track_count,
        gap_seconds,
        total_gap_seconds,
    }))
}

pub fn stream_artifact_selection(req: &Request<()>) -> Result<StreamArtifactSelection> {
    StreamArtifactSelection::parse(
        query_param(req, "artifact")
            .or_else(|| query_param(req, "output"))
            .or_else(|| {
                req.headers()
                    .get("x-encodec-artifact")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string)
            })
            .as_deref(),
    )
}

pub async fn parse_request(req: &Request<()>, body: Bytes) -> Result<ParsedEncodeRequest> {
    let mut gap_seconds = query_param(req, "gap_seconds")
        .as_deref()
        .map(parse_gap_seconds)
        .transpose()?
        .unwrap_or(DEFAULT_TRACK_GAP_SECONDS);
    let mut quality = EncodeQuality::parse(query_param(req, "quality").as_deref())?;
    let mut files = Vec::new();

    if let Some(content_type) = req
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    {
        if let Ok(boundary) = multer::parse_boundary(content_type) {
            let multipart_body = body.clone();
            let body_stream =
                stream::once(async move { Ok::<Bytes, std::io::Error>(multipart_body) });
            let mut multipart = Multipart::new(body_stream, boundary);
            while let Some(field) = multipart
                .next_field()
                .await
                .map_err(|error| client_error(format!("invalid multipart body: {error}")))?
            {
                let name = field.name().unwrap_or_default().to_string();
                let filename = field.file_name().map(|value| value.to_string());
                let content_type = field.content_type().map(|value| value.to_string());
                if let Some(filename) = filename {
                    let bytes = field.bytes().await.map_err(|error| {
                        client_error(format!("failed to read multipart field: {error}"))
                    })?;
                    if bytes.is_empty() {
                        continue;
                    }
                    files.push(UploadedFile {
                        filename,
                        content_type,
                        bytes,
                    });
                    continue;
                }

                let value = field.text().await.map_err(|error| {
                    client_error(format!("failed to read multipart text field: {error}"))
                })?;
                match name.as_str() {
                    "gap_seconds" => gap_seconds = parse_gap_seconds(&value)?,
                    "quality" => quality = EncodeQuality::parse(Some(&value))?,
                    _ => {}
                }
            }
        }
    }

    if files.is_empty() {
        if body.is_empty() {
            return Err(client_error("request did not include any audio file"));
        }
        files.push(UploadedFile {
            filename: request_filename(req),
            content_type: req
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string),
            bytes: body,
        });
    }

    let mut digest = Sha256::new();
    for file in &files {
        digest.update(file.filename.as_bytes());
        digest.update([0]);
        digest.update(&file.bytes);
        digest.update([0xff]);
    }
    digest.update(format!("quality={}", quality.as_str()).as_bytes());
    digest.update(format!("gap={:.6}", gap_seconds).as_bytes());
    let audio_key = format!("{:x}", digest.finalize());

    Ok(ParsedEncodeRequest {
        files,
        gap_seconds,
        quality,
        audio_key,
    })
}

pub fn response_headers(artifacts: &EncodeArtifacts) -> Vec<(String, String)> {
    vec![
        ("cache-control".into(), "no-store".into()),
        ("x-cache".into(), "MISS".into()),
        ("x-ecdc-bytes".into(), artifacts.ecdc.len().to_string()),
        ("x-png-size".into(), artifacts.png_side.to_string()),
        (
            "x-duration".into(),
            round_two(artifacts.duration_seconds).to_string(),
        ),
        ("x-device".into(), artifacts.device_label.clone()),
        ("x-png-format".into(), PNG_FORMAT.into()),
        ("x-encodec-model".into(), MODEL_NAME.into()),
        ("x-encodec-lm".into(), "true".into()),
        (
            "x-encodec-quality".into(),
            artifacts.quality.as_str().to_string(),
        ),
        (
            "x-encodec-bandwidth".into(),
            artifacts.quality.bandwidth_kbps().to_string(),
        ),
        ("x-track-count".into(), artifacts.track_count.to_string()),
        (
            "x-gap-seconds".into(),
            round_two(artifacts.gap_seconds).to_string(),
        ),
        (
            "x-gap-seconds-total".into(),
            round_two(artifacts.total_gap_seconds).to_string(),
        ),
        (
            "x-program-duration".into(),
            round_two(artifacts.duration_seconds).to_string(),
        ),
        ("x-audio-key".into(), artifacts.audio_key.clone()),
        ("x-audio-ready".into(), "false".into()),
    ]
}

pub fn headers_to_json(headers: &[(String, String)]) -> Value {
    let mut map = serde_json::Map::new();
    for (key, value) in headers {
        map.insert(key.clone(), Value::String(value.clone()));
    }
    Value::Object(map)
}

fn runtime_cache() -> &'static Mutex<HashMap<String, Arc<Mutex<OnnxEncodeRuntime>>>> {
    ONNX_RUNTIME_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn bundle_name_for_quality(quality: EncodeQuality) -> &'static str {
    match quality {
        EncodeQuality::Standard => "encodec_48khz_6kbps",
        EncodeQuality::Hq => "encodec_48khz_12kbps",
    }
}

fn bundle_dir_for_quality(config: &AppConfig, quality: EncodeQuality) -> PathBuf {
    Path::new(&config.encodec_onnx_bundle_root).join(bundle_name_for_quality(quality))
}

fn execution_target_for_bundle(config: &AppConfig, bundle_dir: &Path) -> Result<ExecutionTarget> {
    let label = config.execution_target_label().trim().to_ascii_lowercase();
    match label.as_str() {
        "cpu" => Ok(ExecutionTarget::Cpu),
        "gpu" | "cuda" => Ok(ExecutionTarget::Cuda {
            device_id: config.encodec_device_id,
        }),
        "tensorrt" | "trt" => Ok(ExecutionTarget::TensorRt {
            device_id: config.encodec_device_id,
            fp16: false,
            engine_cache_path: Some(bundle_dir.join(".trt-cache/engines")),
            timing_cache_path: Some(bundle_dir.join(".trt-cache/timing.cache")),
        }),
        other => {
            bail!("unsupported ENCODEC_EXECUTION_TARGET {other:?}; use cpu, cuda, gpu, or tensorrt")
        }
    }
}

fn load_onnx_runtime(
    config: &AppConfig,
    quality: EncodeQuality,
) -> Result<Arc<Mutex<OnnxEncodeRuntime>>> {
    let bundle_dir = bundle_dir_for_quality(config, quality);
    let target = execution_target_for_bundle(config, &bundle_dir)?;
    let cache_key = format!("{}::{target:?}", bundle_dir.display());

    if let Some(runtime) = runtime_cache()
        .lock()
        .map_err(|_| anyhow::anyhow!("encodec runtime cache mutex poisoned"))?
        .get(&cache_key)
        .cloned()
    {
        return Ok(runtime);
    }

    let frame = OnnxFrameCodec::from_dir(&bundle_dir, target.clone()).with_context(|| {
        format!(
            "failed to load EnCodec ONNX frame bundle from {}",
            bundle_dir.display()
        )
    })?;
    let lm = OnnxLmCodec::from_dir(&bundle_dir, target).with_context(|| {
        format!(
            "failed to load EnCodec ONNX language model bundle from {}",
            bundle_dir.display()
        )
    })?;
    let runtime = Arc::new(Mutex::new(OnnxEncodeRuntime { frame, lm }));

    runtime_cache()
        .lock()
        .map_err(|_| anyhow::anyhow!("encodec runtime cache mutex poisoned"))?
        .insert(cache_key, runtime.clone());

    Ok(runtime)
}

fn prepared_pcm_to_audio(
    prepared: &PreparedEncodeProgram,
    expected_channels: usize,
    expected_sample_rate: usize,
) -> Result<Array3<f32>> {
    anyhow::ensure!(
        prepared.sample_rate as usize == expected_sample_rate,
        "prepared PCM sample rate {} does not match bundle sample rate {}",
        prepared.sample_rate,
        expected_sample_rate
    );
    anyhow::ensure!(
        prepared.channels as usize == expected_channels,
        "prepared PCM channels {} do not match bundle channels {}",
        prepared.channels,
        expected_channels
    );
    anyhow::ensure!(
        prepared.bits_per_sample == 16,
        "encodec-api currently expects canonical s16le PCM, got {} bits",
        prepared.bits_per_sample
    );

    let bytes_per_frame = expected_channels
        .checked_mul(2)
        .ok_or_else(|| anyhow::anyhow!("invalid channel count"))?;
    anyhow::ensure!(
        prepared.pcm_bytes.len() % bytes_per_frame == 0,
        "prepared PCM length {} is not aligned to {} bytes/frame",
        prepared.pcm_bytes.len(),
        bytes_per_frame
    );

    let frames = prepared.pcm_bytes.len() / bytes_per_frame;
    let mut audio = Array3::<f32>::zeros((1, expected_channels, frames));
    for frame in 0..frames {
        let frame_base = frame * bytes_per_frame;
        for channel in 0..expected_channels {
            let offset = frame_base + channel * 2;
            let sample =
                i16::from_le_bytes([prepared.pcm_bytes[offset], prepared.pcm_bytes[offset + 1]]);
            audio[[0, channel, frame]] = sample as f32 / 32768.0;
        }
    }

    Ok(audio)
}

fn encode_with_onnx_runtime(
    config: &AppConfig,
    prepared: &PreparedEncodeProgram,
) -> Result<Vec<u8>> {
    let runtime = load_onnx_runtime(config, prepared.quality)?;
    let mut runtime = runtime
        .lock()
        .map_err(|_| anyhow::anyhow!("encodec runtime mutex poisoned"))?;
    let OnnxEncodeRuntime { frame, lm } = &mut *runtime;
    let meta = frame.metadata().clone();
    let audio = prepared_pcm_to_audio(prepared, meta.channels, meta.sample_rate)?;
    encode_audio_to_ecdc_with_options(
        frame,
        Some(lm),
        &audio,
        None,
        config.encodec_frame_batch_size,
        false,
    )
}

fn encode_with_onnx_runtime_streaming(
    config: &AppConfig,
    prepared: &PreparedEncodeProgram,
    sender: mpsc::UnboundedSender<Bytes>,
) -> Result<Vec<u8>> {
    let runtime = load_onnx_runtime(config, prepared.quality)?;
    let mut runtime = runtime
        .lock()
        .map_err(|_| anyhow::anyhow!("encodec runtime mutex poisoned"))?;
    let OnnxEncodeRuntime { frame, lm } = &mut *runtime;
    let meta = frame.metadata().clone();
    let audio = prepared_pcm_to_audio(prepared, meta.channels, meta.sample_rate)?;
    let mut encoded = Vec::new();
    encode_audio_to_ecdc_stream_with_options(
        frame,
        Some(lm),
        &audio,
        None,
        config.encodec_frame_batch_size,
        false,
        |chunk| {
            encoded.extend_from_slice(chunk);
            let _ = sender.send(Bytes::copy_from_slice(chunk));
            Ok(())
        },
    )?;
    Ok(encoded)
}

async fn run_encodec(config: &AppConfig, prepared: &PreparedEncodeProgram) -> Result<Bytes> {
    let config = config.clone();
    let prepared = prepared.clone();
    tokio::task::spawn_blocking(move || encode_with_onnx_runtime(&config, &prepared))
        .await
        .context("encodec worker task panicked")?
        .map(Bytes::from)
}

pub fn start_streaming_encodec(
    config: &AppConfig,
    prepared: &PreparedEncodeProgram,
) -> StreamingEncodeHandle {
    let config = config.clone();
    let prepared = prepared.clone();
    let (sender, chunks) = mpsc::unbounded_channel();
    let join = tokio::task::spawn_blocking(move || {
        encode_with_onnx_runtime_streaming(&config, &prepared, sender).map(Bytes::from)
    });
    StreamingEncodeHandle { chunks, join }
}

impl StreamingEncodeHandle {
    pub async fn finish(self) -> Result<Bytes> {
        self.join.await.context("encodec worker task panicked")?
    }
}

pub fn build_artifacts_from_ecdc(
    config: &AppConfig,
    prepared: &PreparedEncodeProgram,
    ecdc: Bytes,
) -> Result<EncodeArtifacts> {
    let (png, png_side) = bytes_to_png(&ecdc)?;
    Ok(EncodeArtifacts {
        ecdc,
        png,
        png_side,
        device_label: config.device_label(),
        duration_seconds: prepared.duration_seconds,
        quality: prepared.quality,
        audio_key: prepared.audio_key.clone(),
        track_count: prepared.track_count,
        gap_seconds: prepared.gap_seconds,
        total_gap_seconds: prepared.total_gap_seconds,
    })
}

fn bytes_to_png(data: &[u8]) -> Result<(Bytes, usize)> {
    let side = png_side_for_bytes(data.len());
    let byte_len = side * side * 3;
    let mut rgb = vec![0_u8; byte_len];
    rgb[..data.len()].copy_from_slice(data);
    let mut out = Vec::new();
    let encoder =
        PngEncoder::new_with_quality(&mut out, CompressionType::Best, FilterType::Adaptive);
    encoder
        .write_image(&rgb, side as u32, side as u32, ExtendedColorType::Rgb8)
        .context("failed to encode PNG")?;
    Ok((Bytes::from(out), side))
}

fn png_side_for_bytes(size_bytes: usize) -> usize {
    let pixels = (size_bytes + 2) / 3;
    (pixels as f64).sqrt().ceil() as usize
}

pub fn prepare_request_program(parsed: ParsedEncodeRequest) -> Result<PreparedEncodeProgram> {
    let track_count = parsed.files.len();
    let uploaded = parsed
        .files
        .into_iter()
        .map(|file| UploadedAudioFile {
            filename: file.filename,
            bytes: file.bytes,
        })
        .collect::<Vec<_>>();
    let prepared = prepare_audio_program(&uploaded, parsed.gap_seconds)?;

    Ok(PreparedEncodeProgram {
        pcm_bytes: prepared.pcm_bytes,
        sample_rate: prepared.sample_rate,
        channels: prepared.channels,
        bits_per_sample: prepared.bits_per_sample,
        duration_seconds: prepared.duration_seconds,
        quality: parsed.quality,
        audio_key: parsed.audio_key,
        track_count,
        gap_seconds: parsed.gap_seconds,
        total_gap_seconds: prepared.total_gap_seconds,
    })
}

fn request_filename(req: &Request<()>) -> String {
    if let Some(value) = req
        .headers()
        .get("x-filename")
        .and_then(|value| value.to_str().ok())
    {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    let extension = req
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(extension_for_content_type)
        .unwrap_or(".bin");
    format!("upload{extension}")
}

fn query_param(req: &Request<()>, key: &str) -> Option<String> {
    url::form_urlencoded::parse(req.uri().query().unwrap_or_default().as_bytes())
        .find_map(|(name, value)| (name == key).then(|| value.into_owned()))
}

fn parse_gap_seconds(value: &str) -> Result<f64> {
    let parsed = value
        .trim()
        .parse::<f64>()
        .map_err(|_| client_error("gap_seconds must be a number"))?;
    if !(0.0..=30.0).contains(&parsed) {
        return Err(client_error("gap_seconds must be between 0 and 30"));
    }
    Ok(parsed)
}

fn extension_for_content_type(content_type: &str) -> Option<&'static str> {
    let content_type = content_type.to_ascii_lowercase();
    match content_type.as_str() {
        "audio/mpeg" => Some(".mp3"),
        "audio/wav" | "audio/x-wav" | "audio/wave" => Some(".wav"),
        "audio/flac" => Some(".flac"),
        "audio/mp4" | "audio/x-m4a" => Some(".m4a"),
        "audio/aac" => Some(".aac"),
        "audio/ogg" => Some(".ogg"),
        _ => None,
    }
}

fn insert_header(headers: &mut HeaderMap, name: &'static str, value: String) -> Result<()> {
    headers.insert(
        name,
        HeaderValue::from_str(&value)
            .map_err(|error| anyhow::anyhow!("invalid {name} header value: {error}"))?,
    );
    Ok(())
}

fn required_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| anyhow::anyhow!("missing header {name}"))
}

fn parse_required_header<T>(headers: &HeaderMap, name: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    required_header(headers, name)?
        .parse::<T>()
        .map_err(|error| anyhow::anyhow!("invalid {name} header: {error}"))
}

async fn emit_progress(
    progress: &mut Option<&mut (dyn ProgressSink + Send)>,
    payload: Value,
) -> Result<()> {
    if let Some(sink) = progress.as_deref_mut() {
        sink.send(payload).await?;
    }
    Ok(())
}

fn round_one(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn round_two(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::Request;

    #[test]
    fn stream_artifact_selection_defaults_to_png() {
        let req = Request::builder().uri("/encode/stream").body(()).unwrap();
        assert_eq!(
            stream_artifact_selection(&req).unwrap(),
            StreamArtifactSelection::Png
        );
    }

    #[test]
    fn stream_artifact_selection_accepts_query_and_header() {
        let req = Request::builder()
            .uri("/encode/stream?artifact=both")
            .header("x-encodec-artifact", "ecdc")
            .body(())
            .unwrap();
        assert_eq!(
            stream_artifact_selection(&req).unwrap(),
            StreamArtifactSelection::Both
        );

        let req = Request::builder()
            .uri("/encode/stream")
            .header("x-encodec-artifact", "ecdc")
            .body(())
            .unwrap();
        assert_eq!(
            stream_artifact_selection(&req).unwrap(),
            StreamArtifactSelection::Ecdc
        );
    }

    #[test]
    fn prepared_internal_headers_roundtrip() {
        let prepared = PreparedEncodeProgram {
            pcm_bytes: vec![1, 2, 3, 4],
            sample_rate: 48_000,
            channels: 2,
            bits_per_sample: 16,
            duration_seconds: 12.5,
            quality: EncodeQuality::Hq,
            audio_key: "abc123".into(),
            track_count: 3,
            gap_seconds: 1.5,
            total_gap_seconds: 3.0,
        };

        let mut request = Request::builder()
            .uri("/encode/stream?artifact=both")
            .body(())
            .unwrap();
        apply_prepared_request_headers(request.headers_mut(), &prepared, true).unwrap();

        let decoded = prepared_program_from_internal_request(&request, &prepared.pcm_bytes)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.pcm_bytes, prepared.pcm_bytes);
        assert_eq!(decoded.sample_rate, prepared.sample_rate);
        assert_eq!(decoded.channels, prepared.channels);
        assert_eq!(decoded.bits_per_sample, prepared.bits_per_sample);
        assert_eq!(decoded.duration_seconds, prepared.duration_seconds);
        assert_eq!(decoded.quality, prepared.quality);
        assert_eq!(decoded.audio_key, prepared.audio_key);
        assert_eq!(decoded.track_count, prepared.track_count);
        assert_eq!(decoded.gap_seconds, prepared.gap_seconds);
        assert_eq!(decoded.total_gap_seconds, prepared.total_gap_seconds);
    }
}
