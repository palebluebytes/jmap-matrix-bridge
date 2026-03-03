{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.jmap-bridge;
  # Assuming the package is available in pkgs or we refer to it by path if not in overlay
  # For now, we'll try to use pkgs.callPackage if it's not strictly in pkgs yet.
  # But ideally it should be in the overlay.
  # Let's assume the user will add it to pkgs.
  # Or we can refer to it directly if we know the path, but that's messy.
  # We will assume it will be added to pkgs via overlay or similar.
  # Fallback: callPackage directly.
  package = pkgs.jmap-matrix-bridge;
in
{
  options.services.jmap-bridge = {
    enable = lib.mkEnableOption "JMAP Matrix Bridge";

    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "File containing secrets (like MATRIX_AS_TOKEN)";
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

    username = lib.mkOption {
      type = lib.types.str;
      description = "JMAP Username";
    };

    token = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "JMAP Token/Password (WARNING: visible in world-readable Nix store if set here)";
    };

    matrixUrl = lib.mkOption {
      type = lib.types.str;
      default = "http://127.0.0.1:6167";
      description = "Matrix Homeserver URL";
    };

    # Registration Configuration
    registration = {
      enable = lib.mkEnableOption "Generate registration.yaml via sops.templates";

      asToken = lib.mkOption {
        type = lib.types.str;
        description = "Application Service Token (pass config.sops.placeholder...)";
      };

      hsToken = lib.mkOption {
        type = lib.types.str;
        description = "Homeserver Token (pass config.sops.placeholder...)";
      };

      owner = lib.mkOption {
        type = lib.types.str;
        default = "root";
        description = "Owner of the generated registration file (e.g. matrix-synapse user)";
      };

      group = lib.mkOption {
        type = lib.types.str;
        default = "root";
        description = "Group of the generated registration file";
      };

      path = lib.mkOption {
        type = lib.types.path;
        readOnly = true;
        default = config.sops.templates."jmap-registration.yaml".path;
        description = "Path to the generated secure registration file";
      };
    };
  };

  config = lib.mkIf cfg.enable {
    # Expose the path for other modules to consume
    # usage: config.services.jmap-bridge.registration.path

    # Define the template if enabled
    sops.templates."jmap-registration.yaml" = lib.mkIf cfg.registration.enable {
      inherit (cfg.registration) owner;
      inherit (cfg.registration) group;
      content = ''
        id: jmap-bridge
        url: http://127.0.0.1:${toString cfg.port}
        as_token: ${cfg.registration.asToken}
        hs_token: ${cfg.registration.hsToken}
        sender_localpart: _jmap_bot
        namespaces:
          users:
          - exclusive: true
            regex: '@_jmap_.*'
          aliases: []
          rooms: []
      '';
    };

    systemd.services.jmap-bridge = {
      description = "JMAP Matrix Bridge";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];
      environment = {
        DATABASE_URL = "sqlite:${cfg.databaseUrl}";
        JMAP_URL = cfg.url;
        JMAP_USERNAME = cfg.username;
        MATRIX_URL = cfg.matrixUrl;
      }
      // lib.optionalAttrs (cfg.token != "") {
        JMAP_TOKEN = cfg.token;
      };
      serviceConfig = {
        ExecStart = ''
          ${package}/bin/jmap-matrix-bridge run \
            --db "sqlite:${cfg.databaseUrl}" \
            --port ${toString cfg.port}
        '';
        Restart = "always";
        RestartSec = "10s";
        StateDirectory = "jmap-bridge";
        WorkingDirectory = "/var/lib/jmap-bridge";
        EnvironmentFile = lib.mkIf (cfg.environmentFile != null) cfg.environmentFile;
        # Hardening
        DynamicUser = false;
        User = "jmap-bridge";
        Group = "jmap-bridge";
        ReadWritePaths = [ "/var/lib/jmap-bridge" ];
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
      };
    };
    users.users.jmap-bridge = {
      isSystemUser = true;
      group = "jmap-bridge";
      description = "JMAP Matrix Bridge service user";
    };

    users.groups.jmap-bridge = { };

  };
}
