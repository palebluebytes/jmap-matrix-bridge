# Automatic double-puppet for interactive users via shared-secret-auth

Extends [ADR-0002](0002-double-puppet-via-login-token.md). The bridge gains an
**optional** automatic double-puppet path for interactive `login` users: when the
operator configures a homeserver **shared secret** (the shared-secret-auth
mechanism), the bridge **mints a login token** for a local user on their behalf, so
double-puppeting needs no manual `login-matrix <token>` step.

This does not reopen ADR-0002. That ADR rejected the declarative **`as_token`**
method because appservice-namespace masquerading floods every co-resident
appservice with foreign events. Shared-secret-auth is a different mechanism: it
yields **the same kind of login token** `login-matrix` already stores and
`/whoami`-validates — not masquerading — so there is no namespace pollution. The
bridge already auto-double-puppets *declaratively-provisioned* users via
`matrix_password_file`; this closes the remaining gap for **interactive** users.

## Considered Options

- **Declarative `as_token` double-puppet (rejected, per ADR-0002)** — namespace
  pollution across co-resident appservices.
- **Manual `login-matrix` only (status quo, rejected as sole option)** — every
  interactive user must fetch and paste an access token; real onboarding friction.
- **Shared-secret-auth, minting a login token (chosen)** — automatic, but produces
  a login token, so it inherits ADR-0002's clean multi-appservice behavior.

## Consequences

- **Opt-in.** Without a configured shared secret the behavior is unchanged: manual
  `login-matrix`. Operators who don't run a shared-secret-auth module lose nothing.
- **Local-homeserver users only.** Shared-secret-auth is per-homeserver, so a user
  whose Matrix account lives on a different homeserver than the bridge **falls back
  to manual `login-matrix`**. This is inherent to the mechanism, not a shortcut.
- The minted token is persisted exactly as the `login-matrix` token is, and the
  scoped `/sync` auto-accept loop from ADR-0002 applies unchanged.
- New operator config (the shared secret) is sensitive — it must be file/secret
  provisioned, like the other credentials, never inline in argv.
