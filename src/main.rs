use clap::{Parser, Subcommand};
use tracing::info;
use serde::{Deserialize, Serialize};

use jmap_matrix_bridge::{config::{self, Registration}, events, ingest, matrix, sender, store, client_manager};
use std::sync::Arc;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Generate the Matrix registration YAML file
    GenerateRegistration {
        /// URL where this bridge can be reached (e.g. http://localhost:8008)
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
        jmap_username: String,

        /// JMAP Token (Password)
        #[arg(long, env = "JMAP_TOKEN")]
        jmap_token: String,

        /// JMAP URL
        #[arg(long, env = "JMAP_URL")]
        jmap_url: String,

        /// Matrix Homeserver URL
        #[arg(long, env = "MATRIX_URL")]
        matrix_url: String,

        /// Matrix Application Service Token
        #[arg(long, env = "MATRIX_AS_TOKEN")]
        matrix_as_token: String,

        /// Port to listen on
        #[arg(short, long, default_value = "8008", env = "PORT")]
        port: u16,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    
    let cli = Cli::parse();

    match cli.command {
        Commands::GenerateRegistration { url, output } => {
            info!("Generating registration file at {}", output);
            let reg = config::generate_registration(&url);
            let f = std::fs::File::create(&output)?;
            serde_yaml::to_writer(f, &reg)?;
            info!("Registration file created!");
        }
        Commands::Run { config: _, db, jmap_username, jmap_token, jmap_url, matrix_url, matrix_as_token, port } => {
            info!("Starting JMAP Bridge on port {} with db: {}", port, db);
            
            let auth = format!("{}:{}", jmap_username, jmap_token);
            use base64::{Engine as _, engine::general_purpose};
            let encoded = general_purpose::STANDARD.encode(auth);
            
            let store = store::Store::new(&db).await?;
            let matrix = matrix::MatrixClient::new(&matrix_url, &matrix_as_token);
            let client_manager = Arc::new(client_manager::ClientManager::new(store.clone(), matrix.clone()));
            
            // Register bot user to ensure it exists in Conduit
            if let Err(e) = matrix.ensure_user_exists("_jmap_bot").await {
                tracing::warn!("Failed to ensure bot user exists: {}", e);
            }
            
            // Start manager (loads users from DB)
            client_manager.start().await?;

            // If CLI args provided credentials, auto-register/login a default user?
            // For now, we will log a message that CLI credentials for single-user are deprecated 
            // or we could auto-register valid credentials to a specific matrix ID if provided?
            // Let's just log for now, as we moved to !login.
            // But verify if we want to keep backward compat:
            if !jmap_username.is_empty() && !jmap_token.is_empty() {
                 info!("CLI credentials provided. Attempting to auto-login for @admin:{}", matrix_url); 
                 // We don't know the exact matrix ID format without parsing config, 
                 // but let's assume the user will !login.
                 info!("Legacy CLI credentials are not automatically active. Please use !login in Matrix.");
            }
            
            let state = events::AppState { client_manager };
            
            // Polling is handled by client_manager

            let app = axum::Router::new()
                .route("/", axum::routing::get(|| async { "JMAP Bridge is running!" }))
                .route("/transactions/:txn_id", axum::routing::put(events::handle_transactions))
                .with_state(state);

            let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
            axum::serve(listener, app).await?;
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
