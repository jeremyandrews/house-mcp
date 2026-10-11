use house_mcp::{config::Config, db::Retry};
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    if let Err(e) = run().await {
        tracing::error!("{e}");
        std::process::exit(1);
    }
}

/// Listens first and connects behind the listener, so a database that is
/// still starting leaves the pod not ready rather than restarting it. The
/// process exits only once the database has stayed out of reach for the
/// whole of `Retry::STARTUP`.
async fn run() -> Result<(), String> {
    let config = Config::from_env()?;
    let listen = config.listen;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|e| format!("binding {listen}: {e}"))?;
    let mut terminate = signal(SignalKind::terminate()).map_err(|e| format!("watching for SIGTERM: {e}"))?;
    let (app, ready) = house_mcp::start(config, Retry::STARTUP);
    tracing::info!("listening on {listen}");
    let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
        tokio::select! {
            _ = terminate.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    });
    let failed = async {
        match ready.await {
            Ok(Ok(())) => std::future::pending().await,
            Ok(Err(e)) => e,
            Err(e) => format!("starting: {e}"),
        }
    };
    tokio::select! {
        served = serve => served.map_err(|e| format!("serving: {e}")),
        e = failed => Err(e),
    }
}
