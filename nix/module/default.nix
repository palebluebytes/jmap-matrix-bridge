{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.jmap-bridge;

  # systemd credential names for user N's JMAP token / Matrix password.
  userCredName = i: "jmap-user-${toString i}";
  userMatrixPwCredName = i: "jmap-matrix-pw-${toString i}";

  # One `--user "…"` argument per declarative user. The token (and optional
  # Matrix password, used for double-puppet auto-join) are referenced by their
  # systemd credential paths so they never appear in argv. systemd only
  # substitutes the `${VAR}` form mid-word (a bare `$VAR` is expanded only as a
  # standalone word), so we emit `${CREDENTIALS_DIRECTORY}` — escaped as `''${`
  # to stop Nix from interpolating it.
  mkUserArg =
    i: u:
    ''--user "mxid=${u.matrixId},username=${u.jmapUsername},url=${
      if u.jmapUrl != null then u.jmapUrl else cfg.url
    },token-file=''${CREDENTIALS_DIRECTORY}/${userCredName i}${
      lib.optionalString (
        u.matrixPasswordFile != null
      ) ",matrix-password-file=\${CREDENTIALS_DIRECTORY}/${userMatrixPwCredName i}"
    }"'';
  userArgsStr = lib.concatStringsSep " " (lib.imap0 mkUserArg cfg.users);
in
{
  options.services.jmap-bridge = {
    enable = lib.mkEnableOption "JMAP Matrix Bridge";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.jmap-matrix-bridge;
      defaultText = lib.literalExpression "pkgs.jmap-matrix-bridge";
      description = "The JMAP Matrix Bridge package to use.";
    };

    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "File containing secrets (like MATRIX_AS_TOKEN)";
    };

    encryptionKeyFile = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "File containing the 32-byte base64 encoded encryption key for credentials at rest";
    };

    databaseUrl = lib.mkOption {
      type = lib.types.str;
      default = "bridge.db?mode=rwc";
      description = "Database URL (defaults to relative path in StateDirectory)";
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 9999;
      description = "Port to listen on";
    };

    url = lib.mkOption {
      type = lib.types.str;
      default = "http://127.0.0.1:8080";
      description = "JMAP Server URL";
    };

    matrixUrl = lib.mkOption {
      type = lib.types.str;
      default = "http://127.0.0.1:6167";
      description = "Matrix Homeserver URL";
    };

    extraArgs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Additional command-line arguments to pass to the bridge service.";
    };

    logLevel = lib.mkOption {
      type = lib.types.str;
      default = "info";
      description = "The logging level for the bridge (error, warn, info, debug, trace)";
    };

    bridgeMailboxes = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Mirror JMAP mailboxes (Inbox/Sent/Drafts/…) as their own Matrix rooms.
        Off by default: email content lives in per-contact/per-thread rooms, so
        the mailbox rooms are just clutter.
      '';
    };

    renderMode = lib.mkOption {
      type = lib.types.enum [
        "plain"
        "links"
        "rich"
      ];
      default = "links";
      description = ''
        How email bodies are rendered into Matrix messages:

        - `plain`: plain text only; links are bare URLs, no buttons.
        - `links` (default): plain text plus a lightweight formatted body that
          keeps clickable links (email buttons become links) but drops images,
          layout and styling.
        - `rich`: plain text plus the full cleaned HTML (images, formatting) —
          closest to the email's real layout, but busier.
      '';
    };

    users = lib.mkOption {
      default = [ ];
      description = ''
        Bridge users to provision declaratively at startup, instead of the
        interactive `!login` flow. Each user's JMAP credentials are connected,
        verified and stored on every start, so this list is the source of truth.
        Tokens are loaded from files via systemd credentials and never appear in
        the process arguments.
      '';
      example = lib.literalExpression ''
        [
          {
            matrixId = "@you:example.com";
            jmapUsername = "you@example.com";
            tokenFile = config.sops.secrets.jmap_token_you.path;
          }
        ]
      '';
      type = lib.types.listOf (
        lib.types.submodule {
          options = {
            matrixId = lib.mkOption {
              type = lib.types.str;
              example = "@you:example.com";
              description = "Full Matrix user id this JMAP account is bridged to.";
            };
            jmapUsername = lib.mkOption {
              type = lib.types.str;
              description = "JMAP username (login) for this account.";
            };
            jmapUrl = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
              description = "JMAP session URL for this user. Defaults to `services.jmap-bridge.url`.";
            };
            tokenFile = lib.mkOption {
              type = lib.types.str;
              description = ''
                Absolute path on the target host to a file containing this user's
                JMAP token/password (e.g. a sops secret path). Must NOT be a Nix
                store path, to avoid exposing the secret.
              '';
            };
            matrixPasswordFile = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
              description = ''
                Optional absolute path on the target host to a file containing
                this user's Matrix account password. When set, the bridge logs in
                as the user to auto-accept the room invites it sends them (so they
                don't manually accept every email room). Must NOT be a Nix store
                path, to avoid exposing the secret.
              '';
            };
          };
        }
      );
    };

  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion =
          cfg.encryptionKeyFile != null
          -> (lib.hasPrefix "/" cfg.encryptionKeyFile && !lib.isStorePath (toString cfg.encryptionKeyFile));
        message = "services.jmap-bridge.encryptionKeyFile must be an absolute path on the target host and NOT a Nix store path (to avoid exposing secrets).";
      }
    ]
    ++ lib.imap0 (i: u: {
      assertion = lib.hasPrefix "/" u.tokenFile && !lib.isStorePath (toString u.tokenFile);
      message = "services.jmap-bridge.users.${toString i}.tokenFile (${u.matrixId}) must be an absolute path on the target host and NOT a Nix store path (to avoid exposing secrets).";
    }) cfg.users
    ++ lib.imap0 (i: u: {
      assertion =
        u.matrixPasswordFile != null
        -> (lib.hasPrefix "/" u.matrixPasswordFile && !lib.isStorePath (toString u.matrixPasswordFile));
      message = "services.jmap-bridge.users.${toString i}.matrixPasswordFile (${u.matrixId}) must be an absolute path on the target host and NOT a Nix store path (to avoid exposing secrets).";
    }) cfg.users;

    systemd.services.jmap-bridge = {
      description = "JMAP Matrix Bridge";
      wantedBy = [ "multi-user.target" ];
      after = [
        "network.target"
        "tuwunel.service"
        "matrix-conduit.service"
        "matrix-synapse.service"
        "stalwart.service"
      ]
      ++ lib.optionals (cfg.encryptionKeyFile != null) [ "sops-install-secrets.service" ];

      wants = lib.optionals (cfg.encryptionKeyFile != null) [ "sops-install-secrets.service" ];

      environment = {
        DATABASE_URL = "sqlite:${cfg.databaseUrl}";
        JMAP_URL = cfg.url;
        MATRIX_URL = cfg.matrixUrl;
        BRIDGE_MAILBOXES = lib.boolToString cfg.bridgeMailboxes;
        RENDER_MODE = cfg.renderMode;
      };

      serviceConfig = {
        ExecStart = ''
          ${cfg.package}/bin/jmap-matrix-bridge --log-level ${cfg.logLevel} run \
            --db "sqlite:${cfg.databaseUrl}" \
            --port ${toString cfg.port} \
            ${
              lib.optionalString (
                cfg.encryptionKeyFile != null
              ) "--encryption-key-file \${CREDENTIALS_DIRECTORY}/encryption-key"
            } \
            ${userArgsStr} \
            ${lib.escapeShellArgs cfg.extraArgs}
        '';
        Restart = "always";
        RestartSec = "10s";

        # State & Directory management
        StateDirectory = "jmap-bridge";
        WorkingDirectory = "/var/lib/jmap-bridge";

        # Secrets / Environment
        EnvironmentFile = lib.mkIf (cfg.environmentFile != null) cfg.environmentFile;
        LoadCredential =
          lib.optionals (cfg.encryptionKeyFile != null) [
            "encryption-key:${cfg.encryptionKeyFile}"
          ]
          # Per-declarative-user JMAP tokens, referenced by token-file in mkUserArg.
          ++ lib.imap0 (i: u: "${userCredName i}:${u.tokenFile}") cfg.users
          # Optional per-user Matrix password for double-puppet auto-join.
          ++ lib.concatLists (
            lib.imap0 (
              i: u:
              lib.optional (u.matrixPasswordFile != null) "${userMatrixPwCredName i}:${u.matrixPasswordFile}"
            ) cfg.users
          );

        # Systemd Hardening & Sandboxing
        DynamicUser = true;
        User = "jmap-bridge";
        Group = "jmap-bridge";

        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        PrivateUsers = true;
        CapabilityBoundingSet = "";
        NoNewPrivileges = true;
        ProtectControlGroups = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        RestrictAddressFamilies = [
          "AF_INET"
          "AF_INET6"
          "AF_UNIX"
        ];
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        MemoryDenyWriteExecute = true;
        LockPersonality = true;
        SystemCallFilter = [
          "@system-service"
          "~@privileged"
        ];
      };
    };

  };
}
