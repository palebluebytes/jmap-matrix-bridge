# JMAP Matrix Bridge — NixOS module

Deploys the `jmap-matrix-bridge` service: a `DynamicUser` systemd unit with state in
`/var/lib/jmap-bridge`, secrets loaded via `LoadCredential` and an optional
`environmentFile`. Users can be provisioned declaratively (see `users` below) or
authenticate themselves at runtime via Matrix commands.

Import it from the flake as `nixosModules.jmap-bridge`; the overlay supplies
`pkgs.jmap-matrix-bridge` as the default package.

## Options (`services.jmap-bridge.*`)

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `enable` | bool | `false` | Enable the service. |
| `package` | package | `pkgs.jmap-matrix-bridge` | Bridge package to run. |
| `url` | str | `"http://127.0.0.1:8080"` | JMAP server URL (passed as `JMAP_URL`; the default JMAP session URL for declarative users). |
| `matrixUrl` | str | `"http://127.0.0.1:6167"` | Matrix homeserver Client-Server API URL. |
| `port` | port | `9999` | TCP port the bridge listens on for homeserver transactions. |
| `databaseUrl` | str | `"bridge.db?mode=rwc"` | SQLite path, relative to the state dir (`/var/lib/jmap-bridge`). Passed as `sqlite:<value>`. |
| `environmentFile` | null or path | `null` | File of `KEY=value` secrets — must define `MATRIX_AS_TOKEN` and `MATRIX_HS_TOKEN`. |
| `encryptionKeyFile` | null or str | `null` | File with a 32-byte base64 AES-256 key; enables credential encryption at rest. |
| `logLevel` | str | `"info"` | `error` \| `warn` \| `info` \| `debug` \| `trace`. |
| `bridgeMailboxes` | bool | `false` | Also mirror JMAP mailboxes as their own Matrix rooms. |
| `renderMode` | enum | `"links"` | Email body rendering: `plain`, `links`, or `rich`. |
| `quoteReplies` | bool | `true` | Quote the parent in outbound replies (email-only). |
| `extraArgs` | list of str | `[]` | Extra args appended to the `run` invocation (e.g. `[ "--matrix-domain" "example.com" ]`). |
| `users` | list of submodule | `[]` | Declaratively provisioned users (see below). |

### `users` submodule

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `matrixId` | str | *required* | Full Matrix user id this account bridges to, e.g. `@you:example.com`. |
| `jmapUsername` | str | *required* | JMAP username (login). |
| `jmapUrl` | null or str | `null` | JMAP session URL for this user; defaults to `services.jmap-bridge.url`. |
| `tokenFile` | str | *required* | Absolute path to a file holding the JMAP token. Must **not** be a Nix store path. |
| `matrixPasswordFile` | null or str | `null` | Absolute path to the Matrix account password; enables double-puppet auto-accept. |

> The homeserver name used to build ghost mxids comes from `--matrix-domain`
> (default `localhost`). If your homeserver isn't `localhost`, set it via
> `extraArgs = [ "--matrix-domain" "example.com" ]` or `MATRIX_DOMAIN` in the
> `environmentFile`.

## Example

```nix
{ config, ... }:
{
  services.jmap-bridge = {
    enable = true;
    matrixUrl = "http://127.0.0.1:6167";
    url = "https://mail.example.com/.well-known/jmap";

    # MATRIX_AS_TOKEN and MATRIX_HS_TOKEN (e.g. from sops-nix)
    environmentFile = config.sops.secrets.jmap_bridge_env.path;
    # 32-byte base64 key; credentials are encrypted at rest when set
    encryptionKeyFile = config.sops.secrets.jmap_bridge_key.path;

    extraArgs = [ "--matrix-domain" "example.com" ];

    users = [{
      matrixId = "@you:example.com";
      jmapUsername = "you@mail.example.com";
      tokenFile = config.sops.secrets.jmap_you_token.path;
      # optional: lets the bridge auto-accept its own room invites as you
      matrixPasswordFile = config.sops.secrets.matrix_you_password.path;
    }];
  };
}
```

## Secrets

- **`environmentFile`** carries the Matrix appservice tokens:
  ```env
  MATRIX_AS_TOKEN=...   # bridge → homeserver
  MATRIX_HS_TOKEN=...   # homeserver → bridge (transaction auth)
  ```
- **`encryptionKeyFile`** and each user's **`tokenFile`** / **`matrixPasswordFile`**
  are passed through systemd `LoadCredential`, so they may live outside the Nix
  store (sops-nix, agenix, plain files with restricted perms).
- **JMAP passwords for self-service users are not configured here** — users supply
  them via the `!login` command and they're stored (encrypted, if a key is set) in
  the SQLite database.

## Registration

This module does **not** generate the Matrix appservice registration. Produce it
once with the bridge's own subcommand and load it into your homeserver:

```bash
jmap-matrix-bridge generate-registration --url http://<host>:9999 --output registration.yaml
```

For tuwunel, drop the file into the configured `appservice_dir`; for
Synapse/Dendrite, reference it from the homeserver config.

## Onboarding

Once running, a user opens a DM with `@_jmap_bot:<server_name>` and runs `login`
(or is provisioned via `users` above). See the [main README](../../README.md#user-guide)
for the full command set.
