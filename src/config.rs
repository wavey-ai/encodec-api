use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use upload_response::UploadResponseConfig;
use web_service::{load_default_tls_base64, load_tls_base64_from_paths};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AppRole {
    Ingress,
    Worker,
}

impl AppRole {
    pub fn serves_encode(self) -> bool {
        matches!(self, Self::Ingress)
    }

    pub fn exposes_upload_cache(self) -> bool {
        matches!(self, Self::Ingress)
    }

    pub fn runs_remote_worker(self) -> bool {
        matches!(self, Self::Worker)
    }
}

#[derive(Debug, Clone, Parser)]
#[command(
    name = "encodec-api",
    about = "Scalable EnCodec API over Wavey's web-service and upload-response stack"
)]
pub struct AppConfig {
    #[arg(long, env = "ENCODEC_API_ROLE", value_enum, default_value_t = AppRole::Ingress)]
    pub role: AppRole,

    #[arg(long, env = "RUST_LOG", default_value = "info,encodec_api=debug")]
    pub rust_log: String,

    #[arg(long, env = "PORT", default_value_t = 8443)]
    pub port: u16,

    #[arg(long, env = "ENABLE_H3", default_value_t = false)]
    pub enable_h3: bool,

    #[arg(long, env = "TLS_CERT_PATH")]
    pub tls_cert_path: Option<String>,

    #[arg(long, env = "TLS_KEY_PATH")]
    pub tls_key_path: Option<String>,

    #[arg(long, env = "ENCODEC_BIN", default_value = "encodec")]
    pub encodec_bin: String,

    #[arg(long, env = "ENCODEC_PYTHON")]
    pub encodec_python: Option<String>,

    #[arg(long, env = "ENCODEC_BACKEND", default_value = "external-encodec")]
    pub encodec_backend: String,

    #[arg(long, env = "ENCODEC_EXECUTION_TARGET")]
    pub encodec_execution_target: Option<String>,

    #[arg(long, env = "UPLOAD_RESPONSE_NUM_STREAMS", default_value_t = 2)]
    pub upload_response_num_streams: usize,

    #[arg(long, env = "UPLOAD_RESPONSE_SLOT_SIZE_KB", default_value_t = 128)]
    pub upload_response_slot_size_kb: usize,

    #[arg(long, env = "UPLOAD_RESPONSE_SLOTS_PER_STREAM", default_value_t = 512)]
    pub upload_response_slots_per_stream: usize,

    #[arg(long, env = "UPLOAD_RESPONSE_TIMEOUT_MS", default_value_t = 300_000)]
    pub upload_response_timeout_ms: u64,

    #[arg(long, env = "UPLOAD_RESPONSE_WATCH_POLL_MS", default_value_t = 2)]
    pub upload_response_watch_poll_ms: u64,

    #[arg(long, env = "UPLOAD_RESPONSE_WORKER_POLL_MS", default_value_t = 10)]
    pub upload_response_worker_poll_ms: u64,

    #[arg(long, env = "UPLOAD_RESPONSE_MAX_INFLIGHT", default_value_t = 1)]
    pub upload_response_max_inflight: usize,

    #[arg(
        long,
        env = "UPLOAD_RESPONSE_WORKER_ID",
        default_value = "encodec-api-worker"
    )]
    pub upload_response_worker_id: String,

    #[arg(long, env = "UPLOAD_RESPONSE_INGRESS_URLS", value_delimiter = ',')]
    pub upload_response_ingress_urls: Vec<String>,

    #[arg(long, env = "UPLOAD_RESPONSE_DISCOVERY_DNS")]
    pub upload_response_discovery_dns: Option<String>,

    #[arg(
        long,
        env = "UPLOAD_RESPONSE_DISCOVERY_INTERVAL_MS",
        default_value_t = 2_000
    )]
    pub upload_response_discovery_interval_ms: u64,

    #[arg(long, env = "UPLOAD_RESPONSE_INSECURE_TLS", default_value_t = false)]
    pub upload_response_insecure_tls: bool,
}

impl AppConfig {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.port > 0, "PORT must be > 0");
        anyhow::ensure!(
            !self.encodec_bin.trim().is_empty(),
            "ENCODEC_BIN must not be empty"
        );
        anyhow::ensure!(
            !self.encodec_backend.trim().is_empty(),
            "ENCODEC_BACKEND must not be empty"
        );
        anyhow::ensure!(
            self.upload_response_num_streams > 0,
            "UPLOAD_RESPONSE_NUM_STREAMS must be > 0"
        );
        anyhow::ensure!(
            self.upload_response_slot_size_kb > 0,
            "UPLOAD_RESPONSE_SLOT_SIZE_KB must be > 0"
        );
        anyhow::ensure!(
            self.upload_response_slots_per_stream > 0,
            "UPLOAD_RESPONSE_SLOTS_PER_STREAM must be > 0"
        );
        anyhow::ensure!(
            self.upload_response_timeout_ms > 0,
            "UPLOAD_RESPONSE_TIMEOUT_MS must be > 0"
        );
        anyhow::ensure!(
            self.upload_response_watch_poll_ms > 0,
            "UPLOAD_RESPONSE_WATCH_POLL_MS must be > 0"
        );
        anyhow::ensure!(
            self.upload_response_worker_poll_ms > 0,
            "UPLOAD_RESPONSE_WORKER_POLL_MS must be > 0"
        );
        anyhow::ensure!(
            self.upload_response_max_inflight > 0,
            "UPLOAD_RESPONSE_MAX_INFLIGHT must be > 0"
        );
        anyhow::ensure!(
            !self.upload_response_worker_id.trim().is_empty(),
            "UPLOAD_RESPONSE_WORKER_ID must not be empty"
        );
        anyhow::ensure!(
            self.upload_response_discovery_interval_ms > 0,
            "UPLOAD_RESPONSE_DISCOVERY_INTERVAL_MS must be > 0"
        );

        if self.role.runs_remote_worker() {
            anyhow::ensure!(
                !self.upload_response_ingress_urls.is_empty()
                    || self
                        .upload_response_discovery_dns
                        .as_ref()
                        .map(|value| !value.trim().is_empty())
                        .unwrap_or(false),
                "worker role requires UPLOAD_RESPONSE_INGRESS_URLS or UPLOAD_RESPONSE_DISCOVERY_DNS"
            );
        }

        Ok(())
    }

    pub fn tls_base64(&self) -> Result<(String, String)> {
        match (&self.tls_cert_path, &self.tls_key_path) {
            (Some(cert_path), Some(key_path)) => load_tls_base64_from_paths(cert_path, key_path)
                .with_context(|| {
                    format!(
                        "failed to load TLS PEMs from {} and {}",
                        cert_path, key_path
                    )
                }),
            (None, None) => load_default_tls_base64()
                .context("failed to load default local Wavey TLS certificate"),
            _ => anyhow::bail!("set both TLS_CERT_PATH and TLS_KEY_PATH, or neither"),
        }
    }

    pub fn upload_response_config(&self) -> UploadResponseConfig {
        UploadResponseConfig {
            num_streams: self.upload_response_num_streams,
            slot_size_kb: self.upload_response_slot_size_kb,
            slots_per_stream: self.upload_response_slots_per_stream,
            response_timeout_ms: self.upload_response_timeout_ms,
        }
    }

    pub fn encodec_backend_label(&self) -> &str {
        self.encodec_backend.as_str()
    }

    pub fn execution_target_label(&self) -> &str {
        self.encodec_execution_target
            .as_deref()
            .unwrap_or_else(|| match self.role {
                AppRole::Ingress => "cpu",
                AppRole::Worker => "gpu",
            })
    }

    pub fn device_label(&self) -> String {
        format!(
            "{}-{}",
            self.execution_target_label(),
            self.encodec_backend_label()
        )
    }
}
