# Bridge access is a default-deny permission map with two levels

The bridge gates who may use it via a `permissions` config map, keyed — most- to
least-specific — by full MXID (`@you:example.com`), homeserver domain
(`example.com`), or `*` (everyone), each mapping to one of two levels:

- **`user`** — may `login`, operate their own JMAP account, and run all
  non-destructive commands.
- **`admin`** — everything `user` can do, plus destructive/global commands
  (`delete-room`, future bulk/diagnostic commands).

Here "admin" is a **User** ([ADR-0009](0009-one-jmap-account-per-matrix-user.md))
granted elevated bridge permissions — never the **Bot** (the appservice's control
user), which the glossary's `_Avoid_: admin` note is about. The two are unrelated.

**Default is deny.** A sender matching no entry may not even `login`. For backward
compatibility, when `permissions` is unset the bridge synthesizes one default
entry: the bridge's own `--matrix-domain` gets `user`, and nobody gets `admin`. So
existing single-homeserver installs keep working with no new config, while
strangers on other federated homeservers — who could previously DM `@_jmap_bot` and
provision a session — are now refused.

The check is the precondition for the command surface: `login` and every command
resolve the sender's level first, and destructive commands require `admin`.

## Considered Options

- **No permissions / open (status quo, rejected)** — anyone on any federated
  homeserver who can message the bot can spin up JMAP clients, create rooms and
  ghosts, and store credentials. Fine for a firewalled single user; a latent
  resource-abuse vector for any reachable deployment.
- **mautrix's four tiers `relay < commands < user < admin` (rejected)** — `relay`
  and `commands` exist for relaybot and read-only-command use-cases this bridge
  doesn't have. Two tiers cover every distinction email needs.
- **Default-deny map with implicit local-domain `user` (chosen)** — closes the
  abuse vector, gives destructive commands a real gate, stays config-driven (no
  runtime `set-pl`), and doesn't break existing single-homeserver installs.

## Consequences

- **A multi-homeserver / public deployment must list permitted domains or MXIDs
  explicitly.** This is the point — open federation access was never intended.
- **`admin` is config-only**, set in the bridge config, not grantable at runtime by
  another admin. Matches the declarative-provisioning posture (no `set-pl` command).
- Membership in `permissions` is orthogonal to having an account: a sender can be
  permitted (`user`) yet not logged in. The permission check runs before the
  login-state machine.
