{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.jmap-bridge;
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

  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion =
          cfg.encryptionKeyFile != null
          -> (lib.hasPrefix "/" cfg.encryptionKeyFile && !lib.isStorePath (toString cfg.encryptionKeyFile));
        message = "services.jmap-bridge.encryptionKeyFile must be an absolute path on the target host and NOT a Nix store path (to avoid exposing secrets).";
      }
    ];

    systemd.services.jmap-bridge = {
      description = "JMAP Matrix Bridge";
      wantedBy = [ "multi-user.target" ];
      after = [
        "network.target"
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
            ${lib.escapeShellArgs cfg.extraArgs}
        '';
        Restart = "always";
        RestartSec = "10s";

        # State & Directory management
        StateDirectory = "jmap-bridge";
        WorkingDirectory = "/var/lib/jmap-bridge";

        # Secrets / Environment
        EnvironmentFile = lib.mkIf (cfg.environmentFile != null) cfg.environmentFile;
        LoadCredential = lib.optionals (cfg.encryptionKeyFile != null) [
          "encryption-key:${cfg.encryptionKeyFile}"
        ];

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
