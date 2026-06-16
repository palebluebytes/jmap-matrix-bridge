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

        /// Mirror JMAP mailboxes (Inbox/Sent/…) as their own Matrix rooms.
        /// Off by default — email lives in per-contact/per-thread rooms.
        #[arg(long, env = "BRIDGE_MAILBOXES", default_value = "false")]
        bridge_mailboxes: bool,

        /// How email bodies render into Matrix messages: `plain` (text only),
        /// `links` (text + clickable links, no images/layout), or `rich`
        /// (full cleaned HTML).
        #[arg(long, env = "RENDER_MODE", default_value = "links")]
        render_mode: String,

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

        /// Declaratively provision a bridge user at startup. Repeatable.
        ///
        /// Value is a comma-separated list of `key=value` pairs. Keys:
        ///   - `mxid`       (required) Matrix user id, e.g. `@you:example.com`
        ///   - `username`   (required) JMAP username
        ///   - `url`        (optional) JMAP session URL; defaults to `--jmap-url`
        ///   - `token-file` (preferred) path to a file holding the JMAP token
        ///   - `token`      (alternative) the JMAP token inline (visible in argv)
        ///
        /// Example:
        ///   --user "mxid=@you:example.com,username=you,token-file=/run/secrets/jmap"
        #[arg(long = "user", value_name = "SPEC")]
        users: Vec<String>,
    },
}

/// A declaratively-provisioned bridge user, parsed from a `--user` spec.
struct UserSpec {
    mxid: String,
    username: String,
    /// JMAP session URL, or `None` to fall back to the global `--jmap-url`.
    url: Option<String>,
    token: String,
    /// Optional Matrix account password, used to log in as this user and
    /// auto-accept the bridge's room invites (double puppeting).
    matrix_password: Option<String>,
}

/// Parse a single `--user` spec (`key=value,key=value,...`).
///
/// The token is taken from `token-file` (read from disk, trimmed) when present,
/// otherwise from an inline `token=` value.
fn parse_user_spec(spec: &str) -> anyhow::Result<UserSpec> {
    let mut mxid = None;
    let mut username = None;
    let mut url = None;
    let mut token = None;
    let mut token_file = None;
    let mut matrix_password_file = None;

    for segment in spec.split(',') {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        let (key, value) = segment
            .split_once('=')
            .with_context(|| format!("invalid --user segment '{segment}', expected key=value"))?;
        let value = value.trim().to_owned();
        match key.trim() {
            "mxid" => mxid = Some(value),
            "username" => username = Some(value),
            "url" => url = Some(value),
            "token" => token = Some(value),
            "token-file" => token_file = Some(value),
            "matrix-password-file" => matrix_password_file = Some(value),
            other => anyhow::bail!("unknown --user key '{other}'"),
        }
    }

    let token = if let Some(path) = token_file {
        std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read --user token-file '{path}'"))?
            .trim()
            .to_owned()
    } else {
        token.context("--user requires either token-file or token")?
    };
    anyhow::ensure!(!token.is_empty(), "--user token is empty");

    let matrix_password = match matrix_password_file {
        Some(path) => {
            let pw = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read --user matrix-password-file '{path}'"))?
                .trim()
                .to_owned();
            (!pw.is_empty()).then_some(pw)
        }
        None => None,
    };

    Ok(UserSpec {
        mxid: mxid.context("--user requires mxid")?,
        username: username.context("--user requires username")?,
        url: url.filter(|u| !u.is_empty()),
        token,
        matrix_password,
    })
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&cli.log_level))
        // html5ever (pulled in by html2text) emits a noisy "foster parenting not
        // implemented" WARN for every messy marketing-HTML table it converts;
        // silence it so it doesn't flood the journal.
        .add_directive(
            "html5ever=error"
                .parse()
                .expect("static directive is valid"),
        );
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
            bridge_mailboxes,
            render_mode,
            matrix_url,
            matrix_as_token,
            matrix_hs_token,
            matrix_domain,
            port,
            encryption_key,
            encryption_key_file,
            users,
        } => {
            info!("Starting JMAP Bridge on port {} with db: {}", port, db);

            let render_mode: jmap_matrix_bridge::services::content::RenderMode = render_mode
                .parse()
                .map_err(|e: String| anyhow::anyhow!(e))?;

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
            let client_manager = Arc::new(
                client_manager::ClientManager::new(store.clone(), matrix.clone(), jmap_sync_limit)
                    .with_bridge_mailboxes(bridge_mailboxes)
                    .with_render_mode(render_mode),
            );

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

            // Matrix double-puppet auto-join manager: runs an auto-accept loop
            // per user that has a Matrix token, so the bridge joins the rooms it
            // invites them to instead of the user clicking "Start chatting".
            let puppet_manager = Arc::new(jmap_matrix_bridge::puppet::PuppetManager::new(
                matrix_url.clone(),
                matrix.bot_user_id().to_string(),
            ));

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

            // Declaratively provision any users passed via --user. This is the
            // multi-user, config-driven path (the spec's token comes from a file
            // so it never lands in argv). Re-running on each boot refreshes the
            // stored credentials from config; a connect failure is non-fatal so
            // a temporary JMAP outage cannot wedge startup.
            for spec in &users {
                match parse_user_spec(spec) {
                    Ok(user) => {
                        let user_url = user.url.unwrap_or_else(|| jmap_url.clone());
                        info!("Provisioning declarative bridge user {}", user.mxid);
                        if let Err(e) = client_manager
                            .login(user.mxid.clone(), user.username, user.token, user_url)
                            .await
                        {
                            tracing::error!(
                                "Failed to provision declarative user {}: {}. Will retry on next start.",
                                user.mxid,
                                e
                            );
                        }
                        // If a Matrix password was configured, log in as the
                        // user to obtain a fresh double-puppet token and start
                        // auto-accepting the bridge's invites for them.
                        if let Some(pw) = &user.matrix_password {
                            match jmap_matrix_bridge::puppet::login_password(
                                &matrix_url,
                                &user.mxid,
                                pw,
                            )
                            .await
                            {
                                Ok(token) => {
                                    if let Err(e) =
                                        store.set_matrix_puppet_token(&user.mxid, &token).await
                                    {
                                        tracing::warn!(
                                            "Failed to store puppet token for {}: {}",
                                            user.mxid,
                                            e
                                        );
                                    }
                                    puppet_manager.ensure_running(user.mxid.clone(), token).await;
                                }
                                Err(e) => tracing::warn!(
                                    "Matrix double-puppet login failed for {}: {}",
                                    user.mxid,
                                    e
                                ),
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Invalid --user spec '{}': {}", spec, e);
                    }
                }
            }

            // Resume double-puppets for any user with a stored Matrix token that
            // isn't already running (interactive `login-matrix` users, plus
            // declarative users whose token was already saved). ensure_running is
            // idempotent, so declarative users started above are not duplicated.
            match store.get_all_users().await {
                Ok(all_users) => {
                    for user in all_users {
                        match store.get_matrix_puppet_token(&user.matrix_user_id).await {
                            Ok(Some(token)) => {
                                puppet_manager
                                    .ensure_running(user.matrix_user_id, token)
                                    .await;
                            }
                            Ok(None) => {}
                            Err(e) => tracing::warn!(
                                "Failed to read puppet token for {}: {}",
                                user.matrix_user_id,
                                e
                            ),
                        }
                    }
                }
                Err(e) => tracing::warn!("Failed to list users to resume puppets: {}", e),
            }

            let state = jmap_matrix_bridge::routes::AppState {
                client_manager: client_manager.clone(),
                state_store,
                puppet_manager: puppet_manager.clone(),
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

    #[test]
    fn test_parse_user_spec_inline_token() {
        let u =
            parse_user_spec("mxid=@you:example.com,username=you,url=https://j/,token=secret")
                .unwrap();
        assert_eq!(u.mxid, "@you:example.com");
        assert_eq!(u.username, "you");
        assert_eq!(u.url.as_deref(), Some("https://j/"));
        assert_eq!(u.token, "secret");
    }

    #[test]
    fn test_parse_user_spec_url_optional_and_whitespace() {
        let u = parse_user_spec(" mxid=@a:b , username=alice , token=tok ").unwrap();
        assert_eq!(u.mxid, "@a:b");
        assert_eq!(u.username, "alice");
        assert_eq!(u.url, None);
        assert_eq!(u.token, "tok");
    }

    #[test]
    fn test_parse_user_spec_token_file() {
        let dir = std::env::temp_dir();
        let path = dir.join("jmap_test_token_spec");
        std::fs::write(&path, "  file-token\n").unwrap();
        let spec = format!(
            "mxid=@x:y,username=x,token-file={}",
            path.to_str().unwrap()
        );
        let u = parse_user_spec(&spec).unwrap();
        assert_eq!(u.token, "file-token");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_parse_user_spec_errors() {
        // Missing required keys.
        assert!(parse_user_spec("username=x,token=t").is_err()); // no mxid
        assert!(parse_user_spec("mxid=@x:y,token=t").is_err()); // no username
        assert!(parse_user_spec("mxid=@x:y,username=x").is_err()); // no token
        // Unknown key and malformed segment.
        assert!(parse_user_spec("mxid=@x:y,username=x,token=t,bogus=1").is_err());
        assert!(parse_user_spec("mxid=@x:y,username,token=t").is_err());
    }
}
