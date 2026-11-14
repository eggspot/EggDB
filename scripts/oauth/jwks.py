import base64
import json
import subprocess
import sys


def b64url(data):
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: jwks.py <RSA private key>")
    modulus = subprocess.run(
        ["openssl", "rsa", "-in", sys.argv[1], "-noout", "-modulus"],
        capture_output=True,
        check=True,
        text=True,
    ).stdout.strip().split("=", 1)[1]
    number = bytes.fromhex(modulus)
    print(json.dumps({"keys": [{"kty": "RSA", "kid": "check", "use": "sig", "alg": "RS256", "n": b64url(number), "e": "AQAB"}]}))


main()
