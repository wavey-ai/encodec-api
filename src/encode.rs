use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use encodec_rs::{Encodec, EncodecOptions};
use futures_util::stream;
use hound::WavReader;
use http::{header::CONTENT_TYPE, Request};
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ExtendedColorType, ImageEncoder};
use multer::Multipart;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use tokio::process::Command;

use crate::config::AppConfig;

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
    pub duration_seconds: f64,
    pub quality: EncodeQuality,
    pub audio_key: String,
    pub track_count: usize,
    pub gap_seconds: f64,
    pub total_gap_seconds: f64,
}

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
    process_parsed_request(config, parsed, progress).await
}

pub async fn process_parsed_request(
    config: &AppConfig,
    parsed: ParsedEncodeRequest,
    progress: &mut Option<&mut (dyn ProgressSink + Send)>,
) -> Result<EncodeArtifacts> {
    emit_progress(
        progress,
        json!({ "type": "Progress", "status": "loading", "msg": "Reading audio..." }),
    )
    .await?;

    let prepared = assemble_program_audio(config, &parsed.files, parsed.gap_seconds).await?;
    emit_progress(
        progress,
        json!({
            "type": "Progress",
            "status": "loading",
            "msg": "Preparing 48kHz stereo...",
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
            "bandwidthKbps": parsed.quality.bandwidth_kbps(),
        }),
    )
    .await?;

    let ecdc = run_encodec(config, &prepared.program_wav, parsed.quality).await?;

    emit_progress(
        progress,
        json!({ "type": "Progress", "status": "packing", "msg": "Packing bytes to RGB..." }),
    )
    .await?;

    let (png, png_side) = bytes_to_png(&ecdc)?;
    let artifacts = EncodeArtifacts {
        ecdc,
        png,
        png_side,
        duration_seconds: prepared.duration_seconds,
        quality: parsed.quality,
        audio_key: parsed.audio_key,
        track_count: parsed.files.len(),
        gap_seconds: parsed.gap_seconds,
        total_gap_seconds: prepared.total_gap_seconds,
    };

    Ok(artifacts)
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
        ("x-device".into(), "external-encodec".into()),
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

struct PreparedProgram {
    _tempdir: TempDir,
    program_wav: PathBuf,
    duration_seconds: f64,
    total_gap_seconds: f64,
}

async fn assemble_program_audio(
    config: &AppConfig,
    files: &[UploadedFile],
    gap_seconds: f64,
) -> Result<PreparedProgram> {
    let tempdir = tempfile::tempdir().context("failed to create tempdir")?;
    let mut normalized = Vec::with_capacity(files.len());

    for (index, file) in files.iter().enumerate() {
        let input_path = tempdir.path().join(format!(
            "input-{index}{}",
            extension_for_filename(&file.filename)
        ));
        tokio::fs::write(&input_path, &file.bytes)
            .await
            .with_context(|| format!("failed to write {}", input_path.display()))?;

        let output_path = tempdir.path().join(format!("normalized-{index}.wav"));
        normalize_audio(&config.ffmpeg_bin, &input_path, &output_path).await?;
        normalized.push(output_path);
    }

    let program_wav = tempdir.path().join("program.wav");
    let total_gap_seconds = if files.len() > 1 {
        gap_seconds.max(0.0) * (files.len() - 1) as f64
    } else {
        0.0
    };

    if normalized.len() == 1 {
        tokio::fs::copy(&normalized[0], &program_wav)
            .await
            .with_context(|| {
                format!("failed to copy {} to program wav", normalized[0].display())
            })?;
    } else {
        let gap_wav = tempdir.path().join("gap.wav");
        if gap_seconds > 0.0 {
            generate_gap_audio(&config.ffmpeg_bin, &gap_wav, gap_seconds).await?;
        }
        let concat_list = tempdir.path().join("concat.txt");
        let mut manifest = String::new();
        for (index, path) in normalized.iter().enumerate() {
            manifest.push_str("file '");
            manifest.push_str(&escape_concat_path(path));
            manifest.push_str("'\n");
            if gap_seconds > 0.0 && index + 1 < normalized.len() {
                manifest.push_str("file '");
                manifest.push_str(&escape_concat_path(&gap_wav));
                manifest.push_str("'\n");
            }
        }
        tokio::fs::write(&concat_list, manifest)
            .await
            .with_context(|| format!("failed to write {}", concat_list.display()))?;
        concat_audio(&config.ffmpeg_bin, &concat_list, &program_wav).await?;
    }

    let duration_seconds = wav_duration_seconds(&program_wav)?;
    Ok(PreparedProgram {
        _tempdir: tempdir,
        program_wav,
        duration_seconds,
        total_gap_seconds,
    })
}

async fn normalize_audio(ffmpeg_bin: &str, input: &Path, output: &Path) -> Result<()> {
    run_command(
        ffmpeg_bin,
        &[
            "-y",
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            &input.display().to_string(),
            "-vn",
            "-ac",
            "2",
            "-ar",
            "48000",
            "-c:a",
            "pcm_s16le",
            &output.display().to_string(),
        ],
    )
    .await
}

async fn generate_gap_audio(ffmpeg_bin: &str, output: &Path, gap_seconds: f64) -> Result<()> {
    run_command(
        ffmpeg_bin,
        &[
            "-y",
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=48000:cl=stereo",
            "-t",
            &format!("{gap_seconds:.6}"),
            "-c:a",
            "pcm_s16le",
            &output.display().to_string(),
        ],
    )
    .await
}

async fn concat_audio(ffmpeg_bin: &str, manifest: &Path, output: &Path) -> Result<()> {
    run_command(
        ffmpeg_bin,
        &[
            "-y",
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "concat",
            "-safe",
            "0",
            "-i",
            &manifest.display().to_string(),
            "-c",
            "copy",
            &output.display().to_string(),
        ],
    )
    .await
}

async fn run_command(program: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("failed to launch {program}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(anyhow::anyhow!(
        "{program} failed with status {}: {}",
        output.status,
        stderr.trim()
    ))
}

async fn run_encodec(
    config: &AppConfig,
    input_wav: &Path,
    quality: EncodeQuality,
) -> Result<Bytes> {
    let output_ecdc = input_wav.with_extension("ecdc");
    let input_path = input_wav.to_path_buf();
    let output_path = output_ecdc.clone();
    let encodec = if let Some(python) = &config.encodec_python {
        Encodec::with_python_module(python)
    } else {
        Encodec::with_binary(config.encodec_bin.clone())
    };

    tokio::task::spawn_blocking(move || {
        encodec.encode_file(
            &input_path,
            &output_path,
            &EncodecOptions {
                bandwidth: Some(quality.bandwidth_kbps()),
                // The scratch.fm path always needs the 48 kHz stereo model.
                high_quality: true,
                language_model: true,
                force: true,
                rescale: false,
            },
        )
    })
    .await
    .context("encodec worker task panicked")?
    .map_err(|error| anyhow::anyhow!(error.to_string()))?;

    let bytes = tokio::fs::read(&output_ecdc)
        .await
        .with_context(|| format!("failed to read {}", output_ecdc.display()))?;
    Ok(Bytes::from(bytes))
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

fn wav_duration_seconds(path: &Path) -> Result<f64> {
    let reader =
        WavReader::open(path).with_context(|| format!("failed to open wav {}", path.display()))?;
    let spec = reader.spec();
    let total_samples = reader.duration() as f64;
    let channels = spec.channels.max(1) as f64;
    Ok(total_samples / spec.sample_rate as f64 / channels)
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

fn extension_for_filename(filename: &str) -> String {
    Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{value}"))
        .unwrap_or_else(|| ".bin".into())
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

fn escape_concat_path(path: &Path) -> String {
    path.display().to_string().replace('\'', "'\\''")
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
}
