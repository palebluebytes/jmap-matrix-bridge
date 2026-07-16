# A bootable NixOS VM playground for the JMAP ↔ Matrix bridge.
#
# It stands up the *same* stack the round-trip test (`nix/check`) uses — a
# Stalwart JMAP mail server, a tuwunel Matrix homeserver, and the bridge itself —
# but as a real, long-lived VM whose Matrix + JMAP ports are forwarded to the
# host, so you can point a normal Matrix client (Element, nheko, …) at it and
# click through the bridge by hand: watch the send-delay ⏳→✅ reactions, edit or
# redact a held message, trip the ❌ failure path, and so on.
#
# It is deliberately insecure (plaintext credentials, open-with-token
# registration, no federation, throwaway tokens) — it is a disposable local
# sandbox, NOT a deployment. Boot it with `nix run .#playground` (see
# `nix/playground/README.md`).
{
  self,
  pkgs,
  ...
}:
let
  hsPort = 8008; # Matrix Client-Server API (forwarded to the host)
  jmapPort = 8081; # Stalwart JMAP (forwarded to the host, handy for curl)
  mgmtPort = 8082; # Stalwart admin API (guest-only)
  bridgePort = 9999; # bridge appservice listener (guest-only)

  # The Matrix account you log in as. It is auto-registered at boot (see
  # matrix-provision below), and the bridge is pre-provisioned for exactly this
  # mxid — so in the GUI you only ever type the homeserver, this localpart and
  # this password. `registrationToken` also gates in-client registration if you
  # want to make more accounts by hand.
  humanLocalpart = "you";
  humanMxid = "@${humanLocalpart}:localhost";
  humanPass = "playground";
  registrationToken = "playground";

  # The mail domain. It MUST be a real, dotted TLD: Stalwart rejects both
  # `localhost` and reserved TLDs like `.test` as "Invalid e-mail address" when
  # creating a sending identity, which silently breaks outbound send. example.com
  # is a normal domain to Stalwart's validator, yet stays entirely local (we make
  # it a local domain below), so mail never leaves the VM.
  mailDomain = "example.com";

  # The Stalwart mail account the bridge drives on your behalf. `jmapUser` is the
  # login name; `bridgeAddr` is its address (and the From on everything it sends).
  jmapUser = "bridgeuser";
  jmapPass = "bridgepass";
  bridgeAddr = "${jmapUser}@${mailDomain}";

  # A second local mail account that plays "the contact" — the seeded email comes
  # *from* it, and your Matrix replies are delivered *to* it (locally, so a real
  # send round-trips without touching the outside network). You can log into it
  # with any JMAP/IMAP client, or just curl its Inbox, to watch replies arrive.
  contactUser = "alice";
  contactPass = "alicepass";
  contactAddr = "${contactUser}@${mailDomain}";

  # Appservice registration — shared verbatim with the bridge's env tokens below.
  # Mirrors nix/check/default.nix so the two stay recognisably the same stack.
  asToken = "secret_as_token";
  hsToken = "secret_hs_token";
  registrationYaml = ''
    id: jmap-bridge
    url: http://127.0.0.1:${toString bridgePort}
    as_token: ${asToken}
    hs_token: ${hsToken}
    sender_localpart: _jmap_bot
    namespaces:
      users:
      - exclusive: true
        regex: '@_jmap_.*'
      aliases: []
      rooms: []
  '';

  # The bridge module wants the JMAP token in a file that is NOT a Nix store path
  # (it asserts this to keep real secrets off world-readable store paths). In the
  # playground the "secret" is a throwaway, so we just drop it at a fixed /etc
  # path — the store symlink target is what systemd LoadCredential reads.
  jmapTokenPath = "/etc/jmap-bridge/${jmapUser}-token";
in
{
  imports = [ ../module ];

  # Pull the bridge package built by this flake into the VM's nixpkgs so
  # `services.jmap-bridge` (whose `package` default is pkgs.jmap-matrix-bridge)
  # resolves to *this* checkout, not a released version.
  # nheko/element-desktop still pull in libolm, which nixpkgs marks insecure.
  # This is a throwaway local sandbox, so allow it.
  nixpkgs.config.permittedInsecurePackages = [ "olm-3.2.16" ];

  nixpkgs.overlays = [
    (_final: prev: {
      jmap-matrix-bridge = self.packages.${prev.stdenv.hostPlatform.system}.jmap-matrix-bridge;
    })
  ];

  networking.hostName = "jmap-playground";

  # ── Matrix homeserver (tuwunel) ────────────────────────────────────────────
  # Conduit-class RAM, loads the appservice registration declaratively from a
  # directory, and needs no signing-key dance. Listens on 0.0.0.0 so QEMU's
  # forwarded host port reaches it. Encryption is OFF: the bridge does not do
  # end-to-bridge encryption yet (ADR-0013), so an E2EE room would be unreadable
  # to it — disabling it keeps every bridge room in cleartext.
  services.matrix-tuwunel = {
    enable = true;
    settings.global = {
      server_name = "localhost";
      address = [ "0.0.0.0" ];
      port = [ hsPort ];
      appservice_dir = "/etc/tuwunel/appservices/";
      allow_federation = false;
      allow_encryption = false;
      allow_registration = true;
      registration_token = registrationToken;
    };
  };
  environment.etc."tuwunel/appservices/jmap-registration.yaml".text = registrationYaml;

  # ── Stalwart JMAP mail server ──────────────────────────────────────────────
  # Wiring mirrors nix/check/default.nix (internal directory + db storage). The
  # JMAP listener is reachable from the host; the management API stays loopback.
  services.stalwart = {
    enable = true;
    stateVersion = "23.11";
    settings = {
      server.hostname = "localhost";
      server.listener = {
        "jmap" = {
          bind = [ "[::]:${toString jmapPort}" ];
          protocol = "http";
        };
        "management" = {
          bind = [ "127.0.0.1:${toString mgmtPort}" ];
          protocol = "http";
        };
      };
      authentication.fallback-admin = {
        user = "admin";
        secret = "admin_password";
      };
      authentication.mechanisms = [ "plain" ];
      authentication.directory = "internal";
      storage = {
        directory = "internal";
        data = "db";
        blob = "db";
        lookup = "db";
        fts = "db";
      };
      directory."internal" = {
        store = "db";
        type = "internal";
      };
    };
  };

  # ── Provision the Stalwart account (+ seed one inbound email) ───────────────
  # The fallback-admin has no JMAP mailbox, so — exactly as the round-trip test
  # does — we create a real domain + individual (with the `user` role that grants
  # JMAP access) via the management API. Then we inject one email from
  # alice@example.com so a ghost room is waiting for you the moment you log in,
  # ready to reply into and watch the send-delay flow. Ordered before the bridge
  # so its declarative login finds a live mailbox.
  systemd.services.stalwart-provision = {
    description = "Provision the playground Stalwart mail account";
    after = [ "stalwart.service" ];
    requires = [ "stalwart.service" ];
    before = [ "jmap-bridge.service" ];
    wantedBy = [ "multi-user.target" ];
    path = [
      pkgs.curl
      pkgs.jq
      pkgs.coreutils
    ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
    };
    script = ''
      set -euo pipefail
      MGMT="http://127.0.0.1:${toString mgmtPort}"
      JMAP="http://127.0.0.1:${toString jmapPort}"
      ADMIN=(-u admin:admin_password)
      AUTH=(-u ${jmapUser}:${jmapPass})
      HDR=(-H 'Content-Type: application/json')

      # Wait for the management API to accept requests.
      for _ in $(seq 1 60); do
        curl -fsS "''${ADMIN[@]}" "$MGMT/api/principal/admin" >/dev/null 2>&1 && break
        sleep 1
      done

      # Domain + two individuals (the bridge account and the contact it mails).
      # Re-running is harmless: a 409 on an existing principal is swallowed so the
      # unit is idempotent across reboots.
      curl -fsS "''${ADMIN[@]}" -X POST "$MGMT/api/principal" "''${HDR[@]}" \
        -d '{"type":"domain","name":"${mailDomain}"}' || true
      curl -fsS "''${ADMIN[@]}" -X POST "$MGMT/api/principal" "''${HDR[@]}" \
        -d '{"type":"individual","name":"${jmapUser}","secrets":["${jmapPass}"],"emails":["${bridgeAddr}"],"roles":["user"]}' || true
      curl -fsS "''${ADMIN[@]}" -X POST "$MGMT/api/principal" "''${HDR[@]}" \
        -d '{"type":"individual","name":"${contactUser}","secrets":["${contactPass}"],"emails":["${contactAddr}"],"roles":["user"]}' || true

      # Wait until JMAP session + Inbox resolve for the new account.
      acct=""
      for _ in $(seq 1 60); do
        acct=$(curl -fsS "''${AUTH[@]}" "$JMAP/jmap/session" \
          | jq -er '.primaryAccounts["urn:ietf:params:jmap:mail"]' 2>/dev/null || true)
        [ -n "$acct" ] && [ "$acct" != "null" ] && break
        sleep 1
      done
      echo "accountId=$acct"

      # Create the bridge account's sending identity. This is THE step that makes
      # outbound send succeed (✅) instead of being rejected (❌): Stalwart never
      # auto-creates identities, and the bridge binds every EmailSubmission to
      # whatever Identity/get returns — so with no identity, every send is
      # rejected. Idempotent: only create when the account has none yet.
      idcount=$(curl -fsS "''${AUTH[@]}" -X POST "$JMAP/jmap" "''${HDR[@]}" -d "$(jq -nc \
        --arg a "$acct" '{using:["urn:ietf:params:jmap:core","urn:ietf:params:jmap:submission"],
          methodCalls:[["Identity/get",{accountId:$a},"0"]]}')" \
        | jq -r '.methodResponses[0][1].list | length')
      if [ "$idcount" = "0" ]; then
        curl -fsS "''${AUTH[@]}" -X POST "$JMAP/jmap" "''${HDR[@]}" -d "$(jq -nc \
          --arg a "$acct" --arg e "${bridgeAddr}" '{using:["urn:ietf:params:jmap:core","urn:ietf:params:jmap:submission"],
            methodCalls:[["Identity/set",{accountId:$a,create:{i:{name:"Bridge User",email:$e}}},"0"]]}')" >/dev/null
        echo "created sending identity ${bridgeAddr}"
      fi

      inbox=$(curl -fsS "''${AUTH[@]}" -X POST "$JMAP/jmap" "''${HDR[@]}" -d "$(jq -nc \
        --arg a "$acct" '{using:["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],
          methodCalls:[["Mailbox/query",{accountId:$a,filter:{role:"inbox"}},"0"]]}')" \
        | jq -er '.methodResponses[0][1].ids[0]')
      echo "inboxId=$inbox"

      # Seed exactly one inbound email — only if the Inbox is empty, so reboots
      # don't pile up duplicates.
      count=$(curl -fsS "''${AUTH[@]}" -X POST "$JMAP/jmap" "''${HDR[@]}" -d "$(jq -nc \
        --arg a "$acct" --arg m "$inbox" '{using:["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],
          methodCalls:[["Email/query",{accountId:$a,filter:{inMailbox:$m}},"0"]]}')" \
        | jq -r '.methodResponses[0][1].ids | length')
      if [ "$count" = "0" ]; then
        curl -fsS "''${AUTH[@]}" -X POST "$JMAP/jmap" "''${HDR[@]}" -d "$(jq -nc \
          --arg a "$acct" --arg m "$inbox" '{using:["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],
            methodCalls:[["Email/set",{accountId:$a,create:{seed:{
              mailboxIds:{($m):true},
              keywords:{"$seen":false},
              from:[{name:"Alice Tester",email:"${contactAddr}"}],
              to:[{email:"${bridgeAddr}"}],
              subject:"Welcome to the playground",
              bodyStructure:{type:"text/plain",partId:"b1"},
              bodyValues:{b1:{value:"Reply to me and watch the send-delay ⏳→✅ flow."}}
            }}},"0"]]}')" >/dev/null
        echo "seeded inbound email"
      fi
    '';
  };

  # ── The bridge ─────────────────────────────────────────────────────────────
  # Declaratively provisioned for the human mxid, so there is no interactive
  # login step: the bridge connects to Stalwart at boot and starts syncing.
  environment.etc."jmap-bridge/${jmapUser}-token".text = jmapPass;

  services.jmap-bridge = {
    enable = true;
    url = "http://localhost:${toString jmapPort}";
    matrixUrl = "http://127.0.0.1:${toString hsPort}";
    matrixDomain = "localhost";
    port = bridgePort;
    logLevel = "info";
    environmentFile = pkgs.writeText "jmap-bridge-playground-env" ''
      MATRIX_AS_TOKEN=${asToken}
      MATRIX_HS_TOKEN=${hsToken}
      RUST_LOG=info
    '';
    users = [
      {
        matrixId = humanMxid;
        jmapUsername = jmapUser;
        tokenFile = jmapTokenPath;
      }
    ];
  };

  # Start the bridge only after the mailbox exists (the provision oneshot).
  systemd.services.jmap-bridge = {
    after = [ "stalwart-provision.service" ];
    requires = [ "stalwart-provision.service" ];
  };

  # ── Register the human Matrix account at boot ──────────────────────────────
  # So the GUI login is just homeserver + localpart + password, with no
  # registration-token/UIA dance. Idempotent: a second boot logs in to confirm
  # the account exists and does nothing. Non-fatal — if it can't register you can
  # still register by hand in the client (the token above still applies).
  systemd.services.matrix-provision = {
    description = "Register the playground human Matrix account";
    after = [ "tuwunel.service" ];
    wants = [ "tuwunel.service" ];
    wantedBy = [ "multi-user.target" ];
    path = [ pkgs.python3 ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
    };
    script = ''
      python3 ${./register-user.py} \
        "http://127.0.0.1:${toString hsPort}" \
        "${humanLocalpart}" "${humanPass}" "${registrationToken}" || \
        echo "matrix-provision: registration failed; register in-client instead"
    '';
  };

  # ── Graphical desktop + a Matrix client (the "GUI") ────────────────────────
  # A lightweight XFCE session that auto-logs-in the `tester` user and auto-opens
  # nheko (a light Qt Matrix client). QEMU shows this desktop in its window. The
  # Qt software backend avoids GL-context failures on QEMU's virtual GPU.
  services.xserver.enable = true;
  services.xserver.desktopManager.xfce.enable = true;
  services.displayManager = {
    autoLogin = {
      enable = true;
      user = "tester";
    };
    defaultSession = "xfce";
  };

  users.users.tester = {
    isNormalUser = true;
    password = "tester";
    extraGroups = [ "wheel" ];
  };

  environment.sessionVariables.QT_QUICK_BACKEND = "software";

  # Auto-launch the Matrix client for the desktop session.
  environment.etc."xdg/autostart/matrix-client.desktop".text = ''
    [Desktop Entry]
    Type=Application
    Name=Matrix client (bridge playground)
    Exec=nheko
    X-GNOME-Autostart-enabled=true
  '';

  # A cheat-sheet on the desktop with the exact login details.
  systemd.tmpfiles.rules = [
    "d /home/tester/Desktop 0755 tester users -"
    "C /home/tester/Desktop/HOW-TO-TEST.txt 0644 tester users - ${pkgs.writeText "how-to-test.txt" ''
      JMAP ↔ Matrix bridge — playground
      ==================================

      A Matrix client (nheko) has opened for you. Log in with:

        Homeserver:  http://localhost:${toString hsPort}
        Username:    ${humanLocalpart}
        Password:    ${humanPass}

      (The account is already registered, and the bridge is already logged in
      to the mail server on your behalf — no `login` step needed.)

      After logging in you'll have invites from @_jmap_bot:
        • a control room, and
        • "Alice Tester (alice@example.com)" — from a seeded email.

      Accept the Alice room and reply in it to test the send-delay flow:
        ⏳  held 5s  →  redact to cancel / edit to rewrite  →  ✅ sent / ❌ failed

      Your reply really sends: the bridge submits it to ${bridgeAddr}'s
      identity and Stalwart delivers it locally to the contact ${contactAddr},
      so you'll see ✅. Watch it actually arrive at the contact's mailbox with:

        curl -sS -u ${contactUser}:${contactPass} http://localhost:${toString jmapPort}/jmap/session

      Prefer Element? Open a terminal and run:  element-desktop
      Bridge logs (open a terminal):            journalctl -u jmap-bridge -f
    ''}"
  ];

  # ── VM shell conveniences ──────────────────────────────────────────────────
  # A passwordless root console (`root`, empty password) for poking at logs and
  # the bridge SQLite DB from the terminal that launched QEMU.
  users.users.root.password = "";
  services.getty.autologinUser = "root";
  environment.systemPackages = with pkgs; [
    curl
    jq
    sqlite
    nheko
    element-desktop
  ];

  # No firewall inside a throwaway loopback VM.
  networking.firewall.enable = false;

  # ── QEMU: graphical window, plus Matrix/JMAP ports forwarded to the host ────
  # `graphics = true` makes QEMU open a desktop window. Ports are still forwarded
  # so you can *also* drive it from a host client or curl if you prefer.
  virtualisation.vmVariant.virtualisation = {
    forwardPorts = [
      {
        from = "host";
        host.port = hsPort;
        guest.port = hsPort;
      }
      {
        from = "host";
        host.port = jmapPort;
        guest.port = jmapPort;
      }
    ];
    memorySize = 4096;
    cores = 4;
    diskSize = 8192;
    graphics = true;
  };

  system.stateVersion = "24.05";
}
