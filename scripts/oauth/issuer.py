import base64
import http.server
import json
import subprocess
import sys
import time


def b64url(data):
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def signed_token(key_path, issuer, audience, subject):
    header = b64url(json.dumps({"alg": "RS256", "typ": "JWT", "kid": "check"}).encode())
    now = int(time.time())
    claims = {"iss": issuer, "aud": audience, "sub": subject, "iat": now, "exp": now + 3600}
    payload = b64url(json.dumps(claims).encode())
    signing_input = f"{header}.{payload}".encode()
    signature = subprocess.run(
        ["openssl", "dgst", "-sha256", "-sign", key_path],
        input=signing_input,
        capture_output=True,
        check=True,
    ).stdout
    return f"{header}.{payload}.{b64url(signature)}"


def main():
    if len(sys.argv) != 6:
        raise SystemExit("usage: issuer.py <port> <signing key> <audience> <subject> <ready file>")
    port, key_path, audience, subject, ready = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5]
    issuer = f"http://127.0.0.1:{port}"

    class Issuer(http.server.BaseHTTPRequestHandler):
        def answer(self, status, document):
            body = json.dumps(document).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            if self.path == "/.well-known/openid-configuration":
                self.answer(200, {
                    "issuer": issuer,
                    "device_authorization_endpoint": f"{issuer}/device",
                    "token_endpoint": f"{issuer}/token",
                    "grant_types_supported": ["urn:ietf:params:oauth:grant-type:device_code"],
                })
            else:
                self.answer(404, {"error": "not_found"})

        def do_POST(self):
            length = int(self.headers.get("Content-Length") or 0)
            self.rfile.read(length)
            if self.path == "/device":
                self.answer(200, {
                    "device_code": "eggdb-check-device-code",
                    "user_code": "EGGD-BCHK",
                    "verification_uri": f"{issuer}/verify",
                    "expires_in": 300,
                    "interval": 1,
                })
            elif self.path == "/token":
                self.answer(200, {
                    "access_token": signed_token(key_path, issuer, audience, subject),
                    "token_type": "Bearer",
                    "expires_in": 3600,
                })
            else:
                self.answer(404, {"error": "not_found"})

        def log_message(self, format, *args):
            sys.stderr.write(format % args + "\n")

    server = http.server.HTTPServer(("127.0.0.1", port), Issuer)
    with open(ready, "w") as marker:
        marker.write(issuer)
    server.serve_forever()


main()
