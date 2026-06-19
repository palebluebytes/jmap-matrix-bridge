{
  self,
  pkgs,
  inputs,
  # Which Matrix homeserver to run the round-trip against: "dendrite" (proven)
  # or "tuwunel" (conduwuit-lineage, Conduit-class RAM, declarative appservices
  # via appservice_dir). The bridge + Stalwart + assertions are identical; only
  # the homeserver service and how it loads the appservice registration differ.
  homeserver ? "dendrite",
  ...
}:

let
  isTuwunel = homeserver == "tuwunel";
  hsUnit = if isTuwunel then "tuwunel" else "dendrite";
  hsPort = 8008;

  # Appservice registration, shared verbatim by both homeservers.
  registrationYaml = ''
    id: jmap-bridge
    url: http://127.0.0.1:9999
    as_token: secret_as_token
    hs_token: secret_hs_token
    sender_localpart: _jmap_bot
    namespaces:
      users:
      - exclusive: true
        regex: '@_jmap_.*'
      aliases: []
      rooms: []
  '';
in
pkgs.testers.nixosTest {
  name = "jmap-bridge-${homeserver}-test";

  nodes.machine =
    {
      lib,
      pkgs,
      ...
    }:
    let
      # Homeserver-specific config. Both listen on hsPort and load the SAME
      # registration; dendrite needs a generated signing key, tuwunel reads the
      # registration from a directory (appservice_dir).
      hsConfig =
        if isTuwunel then
          {
            services.matrix-tuwunel = {
              enable = true;
              settings.global = {
                server_name = "localhost";
                address = [ "127.0.0.1" ];
                port = [ hsPort ];
                appservice_dir = "/etc/tuwunel/appservices/";
                allow_federation = false;
                allow_registration = false;
              };
            };
            environment.etc."tuwunel/appservices/jmap-registration.yaml".text = registrationYaml;
          }
        else
          {
            services.dendrite = {
              enable = true;
              httpPort = hsPort;
              settings = {
                global = {
                  server_name = "localhost";
                  private_key = "/var/lib/dendrite/matrix_key.pem";
                };
                client_api.registration_disabled = true;
                app_service_api.config_files = [ "/etc/dendrite/jmap-registration.yaml" ];
              };
            };
            environment.etc."dendrite/jmap-registration.yaml".text = registrationYaml;
            system.activationScripts.create-dendrite-key = ''
              mkdir -p /var/lib/dendrite
              if [ ! -f /var/lib/dendrite/matrix_key.pem ]; then
                ${pkgs.dendrite}/bin/generate-keys --private-key /var/lib/dendrite/matrix_key.pem
              fi
              chown dendrite:dendrite /var/lib/dendrite/matrix_key.pem
            '';
          };
    in
    {
      imports = [
        inputs.sops-nix.nixosModules.sops
        ../module
      ];

      config = lib.mkMerge [
        hsConfig
        {
          # Satisfy sops assertion
          sops.age.keyFile = "/etc/dummy-sops-key";
          sops.validateSopsFiles = false;

          system.activationScripts.create-dummy-sops-key = ''
            mkdir -p /etc
            echo "AGE-SECRET-KEY-1H6VNY7V4QW7Z8E4G9Q8Z5Z5Z5Z5Z5Z5Z5Z5Z5Z5Z5Z5Z5Z5Z5Z5SXXXXX" > /etc/dummy-sops-key
          '';

          system.activationScripts.create-jmap-bridge-key = ''
            mkdir -p /etc
            echo -n "MTIzNDU2Nzg5MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTI=" > /etc/jmap-bridge-key
          '';

          # Enable Bridge
          services.jmap-bridge = {
            enable = true;
            # Talk to Stalwart directly. The bridge must follow Stalwart's
            # /.well-known/jmap -> /jmap/session redirect itself (no discovery
            # proxy), so this exercises the real session-discovery path.
            url = "http://localhost:8081";
            matrixUrl = "http://127.0.0.1:${toString hsPort}";
            encryptionKeyFile = "/etc/jmap-bridge-key";
            extraArgs = [
              "--jmap-username"
              "bridgeuser"
            ];
            environmentFile = pkgs.writeText "jmap-bridge-env" ''
              MATRIX_AS_TOKEN=secret_as_token
              MATRIX_HS_TOKEN=secret_hs_token
              JMAP_TOKEN=bridgepass
              RUST_LOG=info
            '';
          };

          # Tools used by the round-trip test assertions.
          environment.systemPackages = [
            pkgs.sqlite
            pkgs.jq
          ];

          # Do not auto-start the bridge at boot: the testScript creates the
          # Stalwart account first, then starts the bridge so its one-shot
          # auto-login (main.rs) succeeds against a real mailbox.
          systemd.services.jmap-bridge.wantedBy = lib.mkForce [ ];

          # Stalwart Mail Server
          services.stalwart = {
            enable = true;
            stateVersion = "23.11"; # Or inherit (config.system) stateVersion;
            settings = {
              server.hostname = "localhost";
              server.listener = {
                "jmap" = {
                  bind = [ "[::]:8081" ];
                  protocol = "http";
                };
                # Management/admin API used at test runtime to create a real
                # mail account (the fallback-admin has no JMAP mailbox).
                "management" = {
                  bind = [ "127.0.0.1:8082" ];
                  protocol = "http";
                };
              };

              authentication.fallback-admin = {
                user = "admin";
                secret = "admin_password";
              };

              authentication.mechanisms = [ "plain" ];
              authentication.directory = "internal";

              # Mirror the production mail profile's storage wiring so the
              # internal directory can actually hold accounts + mailboxes.
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

          # Provide the package via overlay
          nixpkgs.overlays = [
            (_final: prev: {
              jmap-matrix-bridge = self.packages.${prev.system}.jmap-matrix-bridge;
            })
          ];

          # Disable nix-command/flakes in the VM to speed up
          nix.settings.experimental-features = lib.mkForce [ ];
        }
      ];
    };

  testScript = ''
    machine.start()

    # Wait for the Matrix homeserver (dendrite or tuwunel)
    machine.wait_for_unit("${hsUnit}.service")
    machine.wait_for_open_port(${toString hsPort})
    machine.wait_until_succeeds("curl -s http://127.0.0.1:${toString hsPort}/_matrix/client/versions")

    print("=== ${hsUnit} LOGS ===")
    print(machine.execute("journalctl -u ${hsUnit}")[1])
    print("=====================")
    # Wait for Stalwart
    try:
        machine.wait_for_unit("stalwart.service")
        machine.wait_for_open_port(8081, timeout=20)
    finally:
        print("=== STALWART LOGS ===")
        print(machine.execute("journalctl -u stalwart")[1])
        print("======================")

    import json
    import urllib.parse

    JMAP = "http://127.0.0.1:8081"   # talk to Stalwart JMAP directly
    MGMT = "http://127.0.0.1:8082"   # Stalwart management/admin API
    AUTH = "-u bridgeuser:bridgepass"
    ADMIN = "-u admin:admin_password"
    HDR = "-H 'Content-Type: application/json'"
    USING = ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"]

    def json_arg(s):
        # Single-quote a JSON string for the shell (JSON never contains single quotes).
        return "'" + s + "'"

    def jmap(*calls):
        # Build a `curl` invocation for a JMAP request as bridgeuser.
        body = json.dumps({"using": USING, "methodCalls": list(calls)})
        return ("curl -sS " + AUTH + " -X POST " + JMAP + "/jmap " + HDR
                + " -d " + json_arg(body))

    # ── Create a real mail account (fallback-admin has no JMAP mailbox) ──────────
    machine.wait_for_open_port(8082, timeout=30)
    print("=== create domain ===")
    print(machine.execute(
        "curl -sS " + ADMIN + " -X POST " + MGMT + "/api/principal " + HDR
        + " -d " + json_arg(json.dumps({"type": "domain", "name": "localhost"})))[1])
    print("=== create individual ===")
    # The `user` role grants the JMAP/IMAP/SMTP permissions. Accounts made via
    # the raw management API do NOT inherit a role by default (the admin UI adds
    # `user` for you), so without this the principal authenticates but gets a
    # 403 "not enough permissions" on /jmap/session.
    print(machine.execute(
        "curl -sS " + ADMIN + " -X POST " + MGMT + "/api/principal " + HDR
        + " -d " + json_arg(json.dumps({
            "type": "individual",
            "name": "bridgeuser",
            "secrets": ["bridgepass"],
            "emails": ["bridgeuser@localhost"],
            "roles": ["user"],
        })))[1])
    print("=== bridgeuser principal (roles/permissions) ===")
    print(machine.execute("curl -sS " + ADMIN + " " + MGMT + "/api/principal/bridgeuser")[1])

    # Gate: the JMAP account must resolve with an Inbox before starting the bridge.
    # Stalwart serves the session at /jmap/session; /.well-known/jmap 307-redirects
    # there, and the bridge must follow that redirect itself (jmap-client trusts the
    # connect host via follow_redirects()).
    print("=== raw JMAP session for bridgeuser ===")
    print(machine.execute("curl -sS -i " + AUTH + " " + JMAP + "/jmap/session")[1])
    account_id = machine.wait_until_succeeds(
        "curl -sS " + AUTH + " " + JMAP + "/jmap/session "
        "| jq -e -r '.primaryAccounts[\"urn:ietf:params:jmap:mail\"]'",
        timeout=60).strip()
    print("accountId=" + account_id)

    inbox_id = machine.wait_until_succeeds(
        "curl -sS " + AUTH + " -X POST " + JMAP + "/jmap " + HDR + " -d " + json_arg(json.dumps({
            "using": USING,
            "methodCalls": [["Mailbox/query", {"accountId": account_id, "filter": {"role": "inbox"}}, "0"]],
        })) + " | jq -e -r '.methodResponses[0][1].ids[0]'",
        timeout=60).strip()
    print("inboxId=" + inbox_id)

    # ── Start the bridge (auto-logs-in @admin:localhost with bridgeuser creds) ───
    machine.succeed("systemctl start jmap-bridge.service")
    machine.wait_for_unit("jmap-bridge.service")
    machine.wait_until_succeeds(
        "journalctl -u jmap-bridge | grep -q 'Subscribed to JMAP EventSource'", timeout=60)

    # ════════════════════════════════════════════════════════════════════════════
    # INBOUND: inject an email into the JMAP Inbox, prove it becomes a Matrix msg.
    # ════════════════════════════════════════════════════════════════════════════
    try:
        created = machine.succeed(
            "curl -sS " + AUTH + " -X POST " + JMAP + "/jmap " + HDR + " -d " + json_arg(json.dumps({
                "using": USING,
                "methodCalls": [["Email/set", {
                    "accountId": account_id,
                    "create": {"inj1": {
                        "mailboxIds": {inbox_id: True},
                        "keywords": {"$seen": False},
                        "from": [{"name": "Alice Tester", "email": "alice@example.com"}],
                        "to": [{"email": "bridgeuser@localhost"}],
                        "subject": "Round-trip probe",
                        "bodyStructure": {"type": "text/plain", "partId": "b1"},
                        "bodyValues": {"b1": {"value": "Hello from JMAP injection"}},
                    }},
                }, "0"]],
            })) + " | tee /dev/stderr | jq -e -r '.methodResponses[0][1].created.inj1.id'")
        print("injected email id=" + created.strip())

        DB = "/var/lib/jmap-bridge/bridge.db"
        have_msg = "sqlite3 " + DB + " 'SELECT COUNT(*) FROM message_mapping;' | grep -qv '^0$'"

        # Primary inbound gate: message_mapping is written only after the Matrix
        # send succeeds (src/sync/email.rs), so a row proves "reached Matrix".
        # First give the push-driven poll a chance; if the JMAP server didn't
        # emit an EventSource change for the injected mail, restart the bridge to
        # force a fresh initial poll (run_event_loop triggers one on startup).
        try:
            machine.wait_until_succeeds(have_msg, timeout=40)
        except Exception:
            print("No push-driven sync within 40s; restarting bridge to force initial poll")
            machine.succeed("systemctl restart jmap-bridge.service")
            machine.wait_until_succeeds(
                "journalctl -u jmap-bridge | grep -q 'Subscribed to JMAP EventSource'", timeout=60)
            machine.wait_until_succeeds(have_msg, timeout=60)
        machine.wait_until_succeeds(
            "sqlite3 " + DB + " \"SELECT COUNT(*) FROM room_ghost_mapping "
            "WHERE ghost_email='alice@example.com';\" | grep -q '^1$'",
            timeout=30)
        print("INBOUND DB assertion passed")

        room_id = machine.succeed(
            "sqlite3 " + DB + " \"SELECT matrix_room_id FROM room_ghost_mapping "
            "WHERE ghost_email='alice@example.com' LIMIT 1;\"").strip()
        print("room_id=" + room_id)

        # Stronger inbound check (best-effort): read the room back from Dendrite,
        # masquerading via the appservice token as the ghost (which is joined).
        ghost = "@_jmap_alice=40example.com:localhost"
        qs = urllib.parse.urlencode({"dir": "b", "limit": "20", "user_id": ghost})
        msgs_url = ("http://127.0.0.1:8008/_matrix/client/v3/rooms/"
                    + urllib.parse.quote(room_id, safe="") + "/messages?" + qs)
        rc, out = machine.execute(
            "curl -sS -H 'Authorization: Bearer secret_as_token' '" + msgs_url + "' "
            "| jq -e '.chunk[] | select(.type==\"m.room.message\") "
            "| select(.content.body | test(\"Hello from JMAP injection\"))'")
        print("DENDRITE inbound check rc=" + str(rc) + " out=" + out)
    finally:
        print("=== BRIDGE LOGS (after inbound) ===")
        print(machine.execute("journalctl -u jmap-bridge")[1])
        print("===================")

    # ════════════════════════════════════════════════════════════════════════════
    # OUTBOUND: forge an appservice transaction (Matrix -> JMAP) into the ghost
    # room and prove the bridge submits an email to JMAP.
    # ════════════════════════════════════════════════════════════════════════════
    try:
        out_txn = json.dumps({"events": [{
            "type": "m.room.message",
            "event_id": "$probeout1",
            "sender": "@admin:localhost",
            "room_id": room_id,
            "origin_server_ts": 1000,
            "content": {"msgtype": "m.text", "body": "Outbound probe reply"},
            "unsigned": {},
        }]})
        machine.succeed(
            "curl -sS -X PUT -H 'Authorization: Bearer secret_hs_token' " + HDR
            + " -d " + json_arg(out_txn)
            + " http://127.0.0.1:9999/_matrix/app/v1/transactions/outbound-probe-1")

        # The outbound path must execute (Matrix transaction -> ghost handler).
        machine.wait_until_succeeds(
            "journalctl -u jmap-bridge | grep -Eq "
            "'Sending fresh email to alice@example.com|Sending ghost room reply'",
            timeout=30)

        # Hard gate: the bridge's JMAP Email/set must have been accepted by
        # Stalwart, i.e. the composed email is now retrievable in the account.
        # submit() (src/sender.rs) files the outgoing copy in the Sent mailbox,
        # so we assert by mailbox membership (reliable -- unlike an FTS subject
        # search, which depends on async full-text indexing).
        sent_id = machine.wait_until_succeeds(
            "curl -sS " + AUTH + " -X POST " + JMAP + "/jmap " + HDR + " -d " + json_arg(json.dumps({
                "using": USING,
                "methodCalls": [["Mailbox/query", {"accountId": account_id, "filter": {"role": "sent"}}, "0"]],
            })) + " | jq -e -r '.methodResponses[0][1].ids[0]'",
            timeout=30).strip()
        print("sentMailboxId=" + sent_id)

        first_id = machine.wait_until_succeeds(
            "curl -sS " + AUTH + " -X POST " + JMAP + "/jmap " + HDR + " -d " + json_arg(json.dumps({
                "using": USING,
                "methodCalls": [["Email/query", {"accountId": account_id, "filter": {"inMailbox": sent_id}}, "0"]],
            })) + " | jq -e -r '.methodResponses[0][1].ids | select(length > 0) | .[0]'",
            timeout=60).strip()
        print("outbound email id in Sent=" + first_id)

        # Prove it is the bridge's message by reading its subject back.
        subject = machine.succeed(
            "curl -sS " + AUTH + " -X POST " + JMAP + "/jmap " + HDR + " -d " + json_arg(json.dumps({
                "using": USING,
                "methodCalls": [["Email/get", {
                    "accountId": account_id, "ids": [first_id], "properties": ["subject", "to"],
                }, "0"]],
            })) + " | jq -r '.methodResponses[0][1].list[0].subject'").strip()
        print("Sent email subject=" + subject)
        print("OUTBOUND JMAP-accept assertion passed")

        # ── QUOTE-REPLIES check ──────────────────────────────────────────────
        # The probe was a threaded reply (it lands in M1's thread, asserted in
        # the THREADING section below), so with quoteReplies on (the module
        # default), reply_to_email (src/sender.rs) must append a standard
        # quoted-original of the parent message (M1) to the outbound body. The
        # quote is an email-layer artifact only -- it never appears in Matrix --
        # so we assert it on the Sent copy, NOT in the Matrix timeline. This is
        # identity-independent (we inspect the email the bridge composed, not the
        # rejected submission), so it holds despite the VM's no-identity limit.
        out_body = machine.succeed(
            "curl -sS " + AUTH + " -X POST " + JMAP + "/jmap " + HDR + " -d " + json_arg(json.dumps({
                "using": USING,
                "methodCalls": [["Email/get", {
                    "accountId": account_id, "ids": [first_id],
                    "properties": ["textBody", "bodyValues"], "fetchTextBodyValues": True,
                }, "0"]],
            })) + " | jq -r '.methodResponses[0][1].list[0].bodyValues[].value'")
        print("Sent email body=" + repr(out_body))
        assert "On " in out_body and "wrote:" in out_body, \
            "outbound reply must carry an 'On ... wrote:' attribution: " + repr(out_body)
        assert "> Hello from JMAP injection" in out_body, \
            "outbound reply must quote the parent message body: " + repr(out_body)
        print("QUOTE-REPLIES assertion passed (outbound reply quotes the parent, email-layer only)")

        # ── SUBMISSION-RESPONSE check ────────────────────────────────────────
        # Filing the copy in Sent (above) is NOT proof of delivery: the separate
        # EmailSubmission/set can still be rejected. submit() (src/sender.rs)
        # therefore inspects the submission response and fails loudly instead of
        # reporting a phantom success -- the regression that silently dropped
        # Matrix->email replies.
        #
        # This VM can only exercise the *rejection* half of that check: Stalwart
        # refuses to provision a sending identity for a management-API account
        # (Identity/set -> "Invalid e-mail address", Identity/get -> []), so the
        # bridge's From is unroutable and every submission is rejected. We assert
        # the bridge SURFACES that (error + retry queue) rather than swallowing
        # it. On a provisioned server (kelpy) the submission is accepted and this
        # branch is simply not hit.
        machine.wait_until_succeeds(
            "journalctl -u jmap-bridge | grep -q 'the JMAP submission was rejected'",
            timeout=30)
        machine.wait_until_succeeds(
            "journalctl -u jmap-bridge | grep -q 'adding to retry queue'",
            timeout=30)
        print("SUBMISSION-RESPONSE assertion passed (rejection surfaced + queued, not silently succeeded)")
    finally:
        print("=== BRIDGE LOGS (after outbound) ===")
        print(machine.execute("journalctl -u jmap-bridge")[1])
        print("===================")

    # ════════════════════════════════════════════════════════════════════════════
    # THREADING: the outbound reply must be a real RFC reply — same JMAP thread as
    # the inbound and referencing its Message-ID — and a contact reply back to it
    # must land in the SAME room, not spawn a new one (per-thread grouping).
    # ════════════════════════════════════════════════════════════════════════════
    try:
        # The injected inbound (M1): its real Message-ID + thread.
        m1 = json.loads(machine.succeed(
            jmap(["Email/get", {"accountId": account_id, "ids": [created.strip()],
                                "properties": ["messageId", "threadId"]}, "0"])
            + " | jq -c '.methodResponses[0][1].list[0]'"))
        m1_msgid = m1["messageId"][0]
        thread_a = m1["threadId"]
        print("M1 messageId=" + m1_msgid + " thread=" + thread_a)

        # The outbound reply must share M1's thread and reference its Message-ID
        # (the reply-threading fix: headers come from the JMAP thread, not the
        # JMAP internal id).
        out = json.loads(machine.succeed(
            jmap(["Email/get", {"accountId": account_id, "ids": [first_id],
                                "properties": ["messageId", "inReplyTo", "references", "threadId"]}, "0"])
            + " | jq -c '.methodResponses[0][1].list[0]'"))
        print("OUT inReplyTo=" + str(out.get("inReplyTo")) + " refs=" + str(out.get("references"))
              + " thread=" + str(out.get("threadId")))
        assert out.get("threadId") == thread_a, \
            "outbound reply must share the inbound thread (got " + str(out.get("threadId")) + ")"
        chain = (out.get("references") or []) + (out.get("inReplyTo") or [])
        assert m1_msgid in chain, "outbound reply must reference the inbound Message-ID"
        out_msgid = out["messageId"][0]
        print("THREADING outbound assertion passed")

        # Contact replies to the bridge's outbound (References the chain, as a
        # real mail client does). Stalwart threads it into thread_a; the bridge
        # must route it into the EXISTING room.
        reply_id = machine.succeed(
            jmap(["Email/set", {
                "accountId": account_id,
                "create": {"inj2": {
                    "mailboxIds": {inbox_id: True},
                    "keywords": {"$seen": False},
                    "from": [{"name": "Alice Tester", "email": "alice@example.com"}],
                    "to": [{"email": "bridgeuser@localhost"}],
                    "subject": "Re: Round-trip probe",
                    "inReplyTo": [out_msgid],
                    "references": [m1_msgid, out_msgid],
                    "bodyStructure": {"type": "text/plain", "partId": "b2"},
                    "bodyValues": {"b2": {"value": "Reply from Alice"}},
                }},
            }, "0"]) + " | jq -e -r '.methodResponses[0][1].created.inj2.id'").strip()
        print("contact reply email id=" + reply_id)

        # The contact reply must reach Matrix. Gate on THIS email specifically
        # (message_mapping is keyed by jmap_email_id) rather than a total row
        # count: this VM cannot grant the bridge a sending identity, so the
        # bridge never learns its own address and cannot drop its own Sent copy
        # as self-authored (kelpy, which has an identity, does drop it). A
        # precise total would therefore race against that extra self-copy row.
        have_reply = ("sqlite3 " + DB + " \"SELECT COUNT(*) FROM message_mapping "
                      "WHERE jmap_email_id='" + reply_id + "';\" | grep -q '^1$'")
        try:
            machine.wait_until_succeeds(have_reply, timeout=40)
        except Exception:
            print("No push-driven sync for the reply within 40s; forcing a poll")
            machine.succeed("systemctl restart jmap-bridge.service")
            machine.wait_until_succeeds(
                "journalctl -u jmap-bridge | grep -q 'Subscribed to JMAP EventSource'", timeout=60)
            machine.wait_until_succeeds(have_reply, timeout=60)

        # The reply joined the existing thread, so there is STILL exactly one room
        # for alice. If the outbound hadn't threaded, the reply would land in a new
        # thread -> a second room here.
        machine.succeed(
            "sqlite3 " + DB + " \"SELECT COUNT(*) FROM room_ghost_mapping "
            "WHERE ghost_email='alice@example.com';\" | grep -q '^1$'")
        print("THREADING same-room assertion passed (reply threaded into the existing room)")
    finally:
        print("=== BRIDGE LOGS (after threading) ===")
        print(machine.execute("journalctl -u jmap-bridge")[1])
        print("===================")
  '';
}
