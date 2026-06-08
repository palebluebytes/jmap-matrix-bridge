#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::str_to_string,
        clippy::too_many_lines,
        clippy::unreadable_literal,
        clippy::uninlined_format_args
    )
)]

use anyhow::Context;
use base64::Engine as _;
use clap::{Parser, Subcommand};
use jmap_matrix_bridge::{client_manager, config, matrix, store};
use std::sync::Arc;
use tracing::info;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Logging level (error, warn, info, debug, trace)
    #[arg(short, long, default_value = "info", env = "LOG_LEVEL")]
    log_level: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum Commands {
    /// Generate the Matrix registration YAML file
    GenerateRegistration {
        /// URL where this bridge can be reached (e.g. <http://localhost:8008>)
        #[arg(short, long)]
        url: String,

        /// Output path for registration file
        #[arg(short, long, default_value = "registration.yaml")]
        output: String,
    },
    /// Run the application service
    Run {
        /// Path to configuration file
        #[arg(short, long, default_value = "config.yaml")]
        config: String,

        /// Database URL
        #[arg(long, default_value = "sqlite:bridge.db", env = "DATABASE_URL")]
        db: String,

        /// JMAP Username
        #[arg(long, env = "JMAP_USERNAME")]
        jmap_username: Option<String>,

        /// JMAP Token (Password)
        #[arg(long, env = "JMAP_TOKEN")]
        jmap_token: Option<String>,

        /// JMAP URL
        #[arg(long, env = "JMAP_URL")]
        jmap_url: String,

        /// JMAP Sync Limit
        #[arg(long, env = "JMAP_SYNC_LIMIT", default_value = "10")]
        jmap_sync_limit: usize,

        /// Matrix Homeserver URL
        #[arg(long, env = "MATRIX_URL")]
        matrix_url: String,

        /// Matrix Application Service Token
        #[arg(long, env = "MATRIX_AS_TOKEN")]
        matrix_as_token: String,

        /// Matrix Homeserver Token (`hs_token`) for transaction endpoint auth
        #[arg(long, env = "MATRIX_HS_TOKEN")]
        matrix_hs_token: String,

        /// Matrix Domain (e.g. palebluebytes.xyz)
        #[arg(long, env = "MATRIX_DOMAIN", default_value = "localhost")]
        matrix_domain: String,

        /// Port to listen on
        #[arg(short, long, default_value = "8008", env = "PORT")]
        port: u16,

        /// AES-256 encryption key (32 bytes, base64-encoded) for encrypting
        /// JMAP credentials at rest. If omitted, credentials are stored in
        /// plain text (legacy mode).
        #[arg(long, env = "ENCRYPTION_KEY")]
        encryption_key: Option<String>,

        /// Path to a file containing the AES-256 encryption key (32 bytes, base64-encoded)
        #[arg(long, env = "ENCRYPTION_KEY_FILE")]
        encryption_key_file: Option<String>,
    },
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&cli.log_level));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    match cli.command {
        Commands::GenerateRegistration { url, output } => {
            info!("Generating registration file at {}", output);
            let reg = config::generate_registration(&url);
            let f = std::fs::File::create(&output)?;
            serde_yaml::to_writer(f, &reg)?;
            info!("Registration file created!");
        }
        Commands::Run {
            config: _,
            db,
            jmap_username,
            jmap_token,
            jmap_url,
            jmap_sync_limit,
            matrix_url,
            matrix_as_token,
            matrix_hs_token,
            matrix_domain,
            port,
            encryption_key,
            encryption_key_file,
        } => {
            info!("Starting JMAP Bridge on port {} with db: {}", port, db);

            // Load key string from encryption_key argument or from the encryption_key_file path
            let key_str = if let Some(key) = encryption_key {
                Some(key)
            } else if let Some(path) = encryption_key_file {
                let content = std::fs::read_to_string(&path)
                    .with_context(|| format!("Failed to read encryption key file from: {path}"))?;
                Some(content.trim().to_owned())
            } else {
                None
            };

            // Parse optional encryption key (hex-encoded 64 chars or base64-encoded 32 bytes)
            let encryption_key: Option<[u8; 32]> = if let Some(key_raw) = key_str {
                let key_trimmed = key_raw.trim();
                let decoded = if key_trimmed.len() == 64
                    && key_trimmed.chars().all(|c| c.is_ascii_hexdigit())
                {
                    let mut bytes = Vec::with_capacity(32);
                    let mut chars = key_trimmed.chars();
                    while let (Some(h), Some(l)) = (chars.next(), chars.next()) {
                        let high = u8::try_from(h.to_digit(16).context("Invalid hex character")?)
                            .map_err(|_| anyhow::anyhow!("Invalid hex digit value"))?;
                        let low = u8::try_from(l.to_digit(16).context("Invalid hex character")?)
                            .map_err(|_| anyhow::anyhow!("Invalid hex digit value"))?;
                        bytes.push((high << 4) | low);
                    }
                    bytes
                } else {
                    base64::engine::general_purpose::STANDARD
                        .decode(key_trimmed)
                        .context(
                            "Encryption key must be valid base64 or a 64-character hex string",
                        )?
                };

                if decoded.len() != 32 {
                    anyhow::bail!(
                        "Encryption key must decode to exactly 32 bytes, got {}",
                        decoded.len()
                    );
                }
                let mut key = [0u8; 32];
                key.copy_from_slice(&decoded);
                Some(key)
            } else {
                None
            };

            let store = store::Store::new(&db, encryption_key).await?;
            let state_store = Arc::new(jmap_matrix_bridge::state::StateStore::new());
            let matrix =
                matrix::MatrixClient::new(&matrix_url, &matrix_as_token, &matrix_domain).await?;
            let client_manager = Arc::new(client_manager::ClientManager::new(
                store.clone(),
                matrix.clone(),
                jmap_sync_limit,
            ));

            // Register bot user to ensure it exists in Conduit
            let mut attempts = 0;
            loop {
                match matrix.ensure_user_exists("_jmap_bot").await {
                    Ok(()) => {
                        info!("Bot user ensured successfully!");
                        break;
                    }
                    Err(e) => {
                        attempts += 1;
                        if attempts >= 10 {
                            tracing::error!(
                                "Failed to ensure bot user exists after 10 attempts: {}",
                                e
                            );
                            anyhow::bail!("Failed to ensure bot user exists: {e}");
                        }
                        tracing::warn!(
                            "Failed to ensure bot user exists (attempt {}): {}. Retrying in 5 seconds...",
                            attempts,
                            e
                        );
                        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    }
                }
            }

            // Set display name and avatar
            let bot_user_id = matrix.bot_user_id();
            if let Err(e) = matrix.set_display_name(&bot_user_id, "JMAP Bridge").await {
                tracing::warn!("Failed to set display name: {}", e);
            }

            let logo_bytes = include_bytes!("../assets/logo.png");
            if let Err(e) = matrix
                .set_avatar(&bot_user_id, logo_bytes, "image/png")
                .await
            {
                tracing::warn!("Failed to set avatar: {}", e);
            }

            // Start manager (loads users from DB)
            client_manager.clone().start().await?;

            // Spawn background database pruning task (runs every 24 hours)
            let pruning_store = store.clone();
            tokio::spawn(async move {
                loop {
                    if let Err(e) = pruning_store.prune_old_data().await {
                        tracing::error!("Failed to prune database: {}", e);
                    }
                    tokio::time::sleep(tokio::time::Duration::from_secs(24 * 3600)).await;
                }
            });

            // Spawn background login state cleanup task (runs every 60 seconds)
            let state_store_cleanup = state_store.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
                loop {
                    interval.tick().await;
                    state_store_cleanup.cleanup_expired().await;
                }
            });

            // Spawn outbound retry worker (runs every 60 seconds)
            tokio::spawn(jmap_matrix_bridge::retry::run_retry_loop(
                store.clone(),
                client_manager.clone(),
                matrix.clone(),
            ));

            // If CLI args provided credentials, auto-register/login a default user?
            // For now, we will log a message that CLI credentials for single-user are deprecated
            // or we could auto-register valid credentials to a specific matrix ID if provided?
            // Let's just log for now, as we moved to !login.
            // But verify if we want to keep backward compat:
            if let (Some(username), Some(token)) = (&jmap_username, &jmap_token) {
                if !username.is_empty() && !token.is_empty() {
                    let target_admin_id = format!("@admin:{matrix_domain}");
                    info!(
                        "CLI credentials provided. Attempting to auto-login for {target_admin_id}"
                    );
                    let target_jmap_url = if jmap_url.is_empty() {
                        "http://127.0.0.1:8080".to_owned()
                    } else {
                        jmap_url.clone()
                    };
                    if let Err(e) = client_manager
                        .login(
                            target_admin_id,
                            username.clone(),
                            token.clone(),
                            target_jmap_url,
                        )
                        .await
                    {
                        tracing::error!("Failed to auto-login CLI user: {}", e);
                    }
                }
            }

            let state = jmap_matrix_bridge::routes::AppState {
                client_manager: client_manager.clone(),
                state_store,
                hs_token: matrix_hs_token,
            };

            // Polling is handled by client_manager

            let app = axum::Router::new()
                .route(
                    "/",
                    axum::routing::get(|| async { "JMAP Bridge is running!" }),
                )
                .route(
                    "/_matrix/app/v1/transactions/{txn_id}",
                    axum::routing::put(jmap_matrix_bridge::routes::handle_transactions),
                )
                .route(
                    "/_matrix/app/v1/users/{user_id}",
                    axum::routing::get(jmap_matrix_bridge::routes::handle_users),
                )
                .route(
                    "/_matrix/app/v1/rooms/{room_alias}",
                    axum::routing::get(jmap_matrix_bridge::routes::handle_rooms),
                )
                .route(
                    "/_matrix/app/v1/ping",
                    axum::routing::post(jmap_matrix_bridge::routes::handle_ping),
                )
                .route_layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    jmap_matrix_bridge::routes::auth_middleware,
                ))
                .with_state(state);

            let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
            let shutdown_manager = client_manager.clone();
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let ctrl_c = async {
                        tokio::signal::ctrl_c()
                            .await
                            .expect("failed to install Ctrl+C handler");
                    };

                    #[cfg(unix)]
                    let terminate = async {
                        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                            .expect("failed to install signal handler")
                            .recv()
                            .await;
                    };

                    #[cfg(not(unix))]
                    let terminate = std::future::pending::<()>();

                    tokio::select! {
                        () = ctrl_c => {},
                        () = terminate => {},
                    }

                    tracing::info!("Shutdown signal received. Initiating graceful shutdown...");
                    shutdown_manager.shutdown().await;
                })
                .await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registration_generation() {
        let reg = config::generate_registration("http://test-url:9000");

        assert_eq!(reg.id, "jmap-bridge");
        assert_eq!(reg.url, "http://test-url:9000");
        assert_eq!(reg.sender_localpart, "_jmap_bot");
        assert_eq!(reg.namespaces.users.len(), 1);
        assert_eq!(reg.namespaces.users[0].regex, "@_jmap_.*");

        // Ensure tokens are generated
        assert_eq!(reg.as_token.len(), 64);
        assert_eq!(reg.hs_token.len(), 64);
        assert_ne!(reg.as_token, reg.hs_token);
    }
}
