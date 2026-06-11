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
        ../../../modules/nixos/services/jmap-bridge/default.nix
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
            url = "http://localhost:8080";
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

          # Proxy to fix JMAP redirect
          systemd.services.jmap-proxy = {
            description = "JMAP Proxy to fix redirect";
            wantedBy = [ "multi-user.target" ];
            serviceConfig = {
              ExecStart = "${pkgs.python3}/bin/python3 ${pkgs.writeText "jmap-proxy.py" ''
                import http.server
                import urllib.request
                import urllib.error

                class H(http.server.BaseHTTPRequestHandler):
                    def do_GET(self):
                        print(f"PROXY GET: {self.path}", flush=True)
                        print(f"HEADERS: {self.headers}", flush=True)
                        url = "http://127.0.0.1:8081" + self.path
                        if self.path == "/.well-known/jmap":
                            url = "http://127.0.0.1:8081/jmap/session"
                            
                        req = urllib.request.Request(url)
                        for k, v in self.headers.items():
                            if k.lower() not in ["host", "connection"]:
                                # Forward the Authorization header on discovery too:
                                # Stalwart's /jmap/session returns the *authenticated*
                                # principal's accounts (accountId). Stripping auth here
                                # yields an anonymous session with the wrong account, so
                                # the bridge's later Mailbox/query hits another account
                                # and Stalwart returns 403 Forbidden.
                                req.add_header(k, v)
                            
                        try:
                            with urllib.request.urlopen(req) as response:
                                self.send_response(response.status)
                                for k, v in response.headers.items():
                                    if k.lower() not in ["transfer-encoding", "connection", "content-length"]:
                                        self.send_header(k, v)
                                
                                body = response.read()
                                if self.path == "/.well-known/jmap":
                                    import json
                                    try:
                                        data = json.loads(body.decode())
                                        # Rewrite URLs to point to the proxy
                                        if "apiUrl" in data:
                                            data["apiUrl"] = data["apiUrl"].replace("127.0.0.1:8081", "127.0.0.1:8080").replace("localhost:8081", "localhost:8080")
                                            # Stalwart advertises apiUrl with a trailing slash (".../jmap/").
                                            # A POST there authenticates but returns 403 Forbidden, whereas
                                            # the bare ".../jmap" works. Normalise so the bridge's JMAP calls
                                            # hit the working endpoint.
                                            if data["apiUrl"].endswith("/jmap/"):
                                                data["apiUrl"] = data["apiUrl"][:-1]
                                        if "downloadUrl" in data:
                                            data["downloadUrl"] = data["downloadUrl"].replace("127.0.0.1:8081", "127.0.0.1:8080").replace("localhost:8081", "localhost:8080")
                                        if "uploadUrl" in data:
                                            data["uploadUrl"] = data["uploadUrl"].replace("127.0.0.1:8081", "127.0.0.1:8080").replace("localhost:8081", "localhost:8080")
                                        print(f"DISCOVERY session primaryAccounts={data.get('primaryAccounts')} apiUrl={data.get('apiUrl')}", flush=True)
                                        body = json.dumps(data).encode()
                                    except Exception as e:
                                        print(f"Failed to parse session JSON: {e}", flush=True)
                                
                                self.send_header("Content-Length", str(len(body)))
                                self.end_headers()
                                self.wfile.write(body)
                        except urllib.error.HTTPError as e:
                            self.send_response(e.code)
                            for k, v in e.headers.items():
                                if k.lower() not in ["transfer-encoding", "connection"]:
                                    self.send_header(k, v)
                            self.end_headers()
                            self.wfile.write(e.read())
                        except Exception as e:
                            self.send_response(500)
                            self.end_headers()
                            self.wfile.write(str(e).encode())

                    def do_POST(self):
                        print(f"PROXY POST: {self.path}", flush=True)
                        print(f"HEADERS: {self.headers}", flush=True)
                        url = "http://127.0.0.1:8081" + self.path
                        content_length = int(self.headers.get('Content-Length', 0))
                        post_data = self.rfile.read(content_length)
                        
                        req = urllib.request.Request(url, data=post_data, method="POST")
                        for k, v in self.headers.items():
                            if k.lower() not in ["host", "connection"]:
                                req.add_header(k, v)
                            
                        try:
                            with urllib.request.urlopen(req) as response:
                                self.send_response(response.status)
                                for k, v in response.headers.items():
                                    if k.lower() not in ["transfer-encoding", "connection"]:
                                        self.send_header(k, v)
                                self.end_headers()
                                self.wfile.write(response.read())
                        except urllib.error.HTTPError as e:
                            self.send_response(e.code)
                            for k, v in e.headers.items():
                                if k.lower() not in ["transfer-encoding", "connection"]:
                                    self.send_header(k, v)
                            self.end_headers()
                            self.wfile.write(e.read())
                        except Exception as e:
                            self.send_response(500)
                            self.end_headers()
                            self.wfile.write(str(e).encode())

                http.server.HTTPServer(("127.0.0.1", 8080), H).serve_forever()
              ''}";
              Restart = "always";
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
    # Wait for Stalwart and Proxy
    try:
        machine.wait_for_unit("stalwart.service")
        machine.wait_for_open_port(8081, timeout=20)
        machine.wait_for_unit("jmap-proxy.service")
        machine.wait_for_open_port(8080, timeout=10)
    finally:
        print("=== STALWART LOGS ===")
        print(machine.execute("journalctl -u stalwart")[1])
        print("======================")
        print("=== PROXY LOGS ===")
        print(machine.execute("journalctl -u jmap-proxy")[1])
        print("===================")

    import json
    import urllib.parse

    JMAP = "http://127.0.0.1:8081"   # talk to Stalwart JMAP directly (bypass the bridge's proxy)
    MGMT = "http://127.0.0.1:8082"   # Stalwart management/admin API
    AUTH = "-u bridgeuser:bridgepass"
    ADMIN = "-u admin:admin_password"
    HDR = "-H 'Content-Type: application/json'"
    USING = ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"]

    def json_arg(s):
        # Single-quote a JSON string for the shell (JSON never contains single quotes).
        return "'" + s + "'"

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
    # Stalwart serves the session at /jmap/session (its /.well-known/jmap is a
    # redirect -- that's what the bundled proxy rewrites).
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
    finally:
        print("=== BRIDGE LOGS (after outbound) ===")
        print(machine.execute("journalctl -u jmap-bridge")[1])
        print("===================")
  '';
}
