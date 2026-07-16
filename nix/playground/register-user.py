#!/usr/bin/env python3
"""Register (or confirm) the playground's human Matrix account.

Usage: register-user.py <base-url> <localpart> <password> <registration-token>

Idempotent and best-effort: if the account already exists it just logs in to
confirm, and any failure is reported via a non-zero exit so the caller can fall
back to in-client registration. Uses only the stdlib so it needs no packaging.
"""

import json
import sys
import time
import urllib.error
import urllib.request

base, localpart, password, token = sys.argv[1:5]


def call(path, body=None, method="GET"):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        base + path,
        data=data,
        headers={"Content-Type": "application/json"},
        method=method,
    )
    try:
        return 200, json.load(urllib.request.urlopen(req))
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.load(e)
        except Exception:
            return e.code, {}


# Wait for the homeserver's Client-Server API to come up.
for _ in range(120):
    try:
        urllib.request.urlopen(base + "/_matrix/client/versions")
        break
    except Exception:
        time.sleep(1)

# Already registered? A successful login means we're done.
code, r = call(
    "/_matrix/client/v3/login",
    {
        "type": "m.login.password",
        "identifier": {"type": "m.id.user", "user": localpart},
        "password": password,
    },
    "POST",
)
if code == 200:
    print(f"matrix-provision: {localpart} already registered")
    sys.exit(0)

# UIA registration. Stage one surfaces the session/flows; stage two supplies the
# registration token; some homeservers then require an m.login.dummy stage.
_, r = call(
    "/_matrix/client/v3/register",
    {"username": localpart, "password": password, "auth": {}},
    "POST",
)
session = r.get("session")

body = {
    "username": localpart,
    "password": password,
    "auth": {
        "type": "m.login.registration_token",
        "token": token,
        **({"session": session} if session else {}),
    },
}
code, r = call("/_matrix/client/v3/register", body, "POST")

if code == 401:  # token accepted, another stage (usually dummy) still required.
    body["auth"] = {"type": "m.login.dummy", "session": r.get("session", session)}
    code, r = call("/_matrix/client/v3/register", body, "POST")

if code == 200 and r.get("user_id"):
    print(f"matrix-provision: registered {r['user_id']}")
    sys.exit(0)

print(f"matrix-provision: registration failed ({code}): {r.get('error', r)}")
sys.exit(1)
