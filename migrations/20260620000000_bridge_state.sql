-- Global, non-user-scoped key/value state for the bridge itself.
--
-- Unlike `jmap_state`, this table has no foreign key to `users`: it holds
-- bridge-wide facts (e.g. the bot's last-applied avatar hash / mxc and
-- display name) so the bot profile is only re-uploaded and re-set when it
-- actually changes, instead of on every startup. Mirrors how mautrix bridges
-- persist `AvatarHash`/`AvatarMXC`/`AvatarSet` to avoid redundant profile
-- writes and orphaned media on the homeserver.
CREATE TABLE IF NOT EXISTS bridge_state (
    state_key TEXT PRIMARY KEY,
    state_value TEXT NOT NULL
) STRICT;
