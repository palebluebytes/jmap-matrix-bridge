{
  self,
  pkgs,
  inputs,
  ...
}:

pkgs.testers.nixosTest {
  name = "jmap-bridge-test";

  nodes.machine =
    {
      lib,
      pkgs,
      ...
    }:
    {
      imports = [
        inputs.sops-nix.nixosModules.sops
        ../../../modules/nixos/services/jmap-bridge/default.nix
      ];

      config = {
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

        # Enable Dendrite
        services.dendrite = {
          enable = true;
          settings = {
            global = {
              server_name = "localhost";
              private_key = "/var/lib/dendrite/matrix_key.pem";
            };
            client_api.registration_disabled = true;
            app_service_api.config_files = [ "/etc/dendrite/jmap-registration.yaml" ];
          };
        };

        # Provide registration file for Application Service
        environment.etc."dendrite/jmap-registration.yaml".text = ''
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

        # Enable Bridge
        services.jmap-bridge = {
          enable = true;
          url = "http://localhost:8080";
          matrixUrl = "http://127.0.0.1:8008"; # Dendrite
          encryptionKeyFile = "/etc/jmap-bridge-key";
          extraArgs = [
            "--jmap-username"
            "admin"
          ];
          environmentFile = pkgs.writeText "jmap-bridge-env" ''
            MATRIX_AS_TOKEN=secret_as_token
            MATRIX_HS_TOKEN=secret_hs_token
            JMAP_TOKEN=admin_password
            RUST_LOG=info
          '';
        };

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
            };

            authentication.fallback-admin = {
              user = "admin";
              secret = "admin_password";
            };

            authentication.mechanisms = [ "plain" ];
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
                              if self.path == "/.well-known/jmap" and k.lower() == "authorization":
                                  print("SKIPPING AUTH HEADER FOR DISCOVERY", flush=True)
                                  continue
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
                                      if "downloadUrl" in data:
                                          data["downloadUrl"] = data["downloadUrl"].replace("127.0.0.1:8081", "127.0.0.1:8080").replace("localhost:8081", "localhost:8080")
                                      if "uploadUrl" in data:
                                          data["uploadUrl"] = data["uploadUrl"].replace("127.0.0.1:8081", "127.0.0.1:8080").replace("localhost:8081", "localhost:8080")
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

        # Create dummy key for Dendrite
        system.activationScripts.create-dendrite-key = ''
          mkdir -p /var/lib/dendrite
          if [ ! -f /var/lib/dendrite/matrix_key.pem ]; then
            ${pkgs.dendrite}/bin/generate-keys --private-key /var/lib/dendrite/matrix_key.pem
          fi
          chown dendrite:dendrite /var/lib/dendrite/matrix_key.pem
        '';

        # Disable nix-command/flakes in the VM to speed up
        nix.settings.experimental-features = lib.mkForce [ ];
      };
    };

  testScript = ''
    machine.start()

    # Wait for Dendrite
    machine.wait_for_unit("dendrite.service")
    machine.wait_for_open_port(8008)
    machine.wait_until_succeeds("curl -s http://127.0.0.1:8008/_matrix/client/versions")

    print("=== DENDRITE LOGS ===")
    print(machine.execute("journalctl -u dendrite")[1])
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

    # Start the bridge
    machine.succeed("systemctl start jmap-bridge.service")
    machine.wait_for_unit("jmap-bridge.service")

    try:
        # Check if the bridge is running and logged something
        machine.wait_until_succeeds("journalctl -u jmap-bridge | grep -i 'Subscribed to JMAP EventSource'", timeout=10)
    finally:
        print("=== BRIDGE LOGS ===")
        print(machine.execute("journalctl -u jmap-bridge")[1])
        print("===================")
  '';
}
