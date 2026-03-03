# JMAP Matrix Bridge Module

This NixOS module deploys the `jmap-matrix-bridge` service, managing the process, environment, and persistence. It supports the multi-user architecture where users authenticate themselves via Matrix commands.

## Options

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `services.jmap-bridge.enable` | bool | `false` | Enable the JMAP Bridge service. |
| `services.jmap-bridge.url` | str | `"http://127.0.0.1:8080/jmap/session"` | **Default JMAP Session URL**. Used as a hint or default for the service. Users provide specific URLs during login. |
| `services.jmap-bridge.matrixUrl` | str | `"http://127.0.0.1:6167"` | URL of the Matrix Homeserver (Client-Server API) for sending events. |
| `services.jmap-bridge.databaseUrl` | str | `"sqlite:/var/lib/jmap-bridge/bridge.db"` | Path to the SQLite database. Ensure the directory exists and is writable. |
| `services.jmap-bridge.environmentFile` | path | `null` | Path to a file containing environment variables (e.g., from `sops-nix`). Critical for secret injection (`MATRIX_AS_TOKEN`). |
| `services.jmap-bridge.username` | str | `null` | **Deprecated**. Static JMAP username for single-user mode. |
| `services.jmap-bridge.registration` | submodule | - | Configuration for generating/managing the App Service registration file. |

## Example Usage

### Multi-User Setup (Recommended)

```nix
{ config, pkgs, ... }:
{
  services.jmap-bridge = {
    enable = true;
    # Matrix server URL (from the bridge's perspective)
    matrixUrl = "http://127.0.0.1:6167";
    
    # Load the App Service Token (MATRIX_AS_TOKEN) from sops
    environmentFile = config.sops.secrets.jmap_bridge_env.path;
    
    # Registration file management
    registration = {
        enable = true;
        asToken = config.sops.placeholder.email_as_token;
        hsToken = config.sops.placeholder.email_hs_token;
        owner = "conduit"; # Ensure your Homeserver can read this
        group = "conduit";
    };
  };
}
```

## Secrets Handling

The bridge needs the **Application Service Token** to communicate with the Matrix Homeserver as a privileged app service.

Define this in your `sops` secrets file and expose it via `environmentFile`:
```env
MATRIX_AS_TOKEN=your_generated_token_here
```

*Note: User JMAP passwords are NOT configured here. They are provided by users via the `!login` command and stored encrypted in the local SQLite database.*

## User Onboarding

Once the bridge is running:
1.  Users open a DM with the bot user (defined in your registration, default `@_jmap_bot:<server_name>`).
2.  Users authenticate:
    ```
    !login <jmap_username> <jmap_password> <jmap_session_url>
    ```
