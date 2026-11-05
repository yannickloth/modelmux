use std::path::PathBuf;
use std::sync::Arc;

use modelmux::{app, state::AppState, VERSION};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("modelmux=info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("modelmux {VERSION}");
        return Ok(());
    }

    let config_path = resolve_config_path(&args)?;
    tracing::info!(path = %config_path.display(), "loading config");
    let state = Arc::new(
        AppState::new(config_path)
            .await
            .map_err(anyhow::Error::msg)?,
    );

    let listen = state.cfg.read().await.server.listen.clone();
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!("modelmux {VERSION} listening on {listen}");
    axum::serve(listener, app(state)).await?;
    Ok(())
}

fn resolve_config_path(args: &[String]) -> anyhow::Result<PathBuf> {
    // --config / -c <path>
    if let Some(i) = args.iter().position(|a| a == "--config" || a == "-c") {
        if let Some(p) = args.get(i + 1) {
            let p = PathBuf::from(p);
            anyhow::ensure!(p.is_file(), "config file not found: {}", p.display());
            return Ok(p);
        }
        anyhow::bail!("--config requires a path argument");
    }
    // $MODELMUX_CONFIG
    if let Ok(p) = std::env::var("MODELMUX_CONFIG") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        anyhow::bail!("MODELMUX_CONFIG points to a missing file: {}", p.display());
    }
    for name in ["modelmux.toml", "config.toml"] {
        let p = PathBuf::from(name);
        if p.is_file() {
            return Ok(p);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home).join(".config/modelmux/config.toml");
        if p.is_file() {
            return Ok(p);
        }
    }
    anyhow::bail!(
        "no config found. Looked at ./modelmux.toml, ./config.toml, $MODELMUX_CONFIG, \
         ~/.config/modelmux/config.toml. Use --config <path> to pass one explicitly."
    )
}
