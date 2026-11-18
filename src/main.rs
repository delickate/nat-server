use std::env;

use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("nat_server=info")),
        )
        .init();

    let bind_addr = env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_owned());
    let config = nat_server::Config::from_env()?;
    let listener = TcpListener::bind(&bind_addr).await?;
    tracing::info!(
        addr = %listener.local_addr()?,
        max_connections = config.max_connections,
        request_timeout_secs = config.request_timeout.as_secs(),
        "nat-server listening"
    );

    axum::serve(listener, nat_server::router(config))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("nat-server stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
