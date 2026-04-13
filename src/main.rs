#[tokio::main]
async fn main() -> anyhow::Result<()> {
    encodec_api::run_from_env().await
}
