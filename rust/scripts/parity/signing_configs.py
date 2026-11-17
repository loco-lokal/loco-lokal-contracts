#!/usr/bin/env python3
"""Compares signed result documents and audit trails under the account's signing settings.

    rust/scripts/parity/signing_configs.py TOKEN TEMPLATE_ID

Needs what `signing.py` needs (both job workers) plus `openssl`. Each setting is stored
through the Rails container (encrypted by Rails), then `signing.py`'s two-party signed
submission runs on both backends, and the setting is restored:

- timestamp servers (`timestamp_server_url`, `Submissions::TimestampHandler`): a local
  RFC 3161 server (`openssl ts -reply`) on TSA_HOST:TSA_PORT (default HOOK_HOST or
  127.0.0.1, port 4598), which both servers must reach; then with basic-auth credentials
  and a failing first URL (the comma-separated fallback answers); then unreachable;
- custom signing certificates: a PKCS#12 file selected as the default certificate, once
  with OpenSSL 3's default encryption (PBES2/AES) and once with the legacy one (RC2, 3DES).

Besides `signing.py`'s comparisons (stored rows, page renders, words, metadata, signature
counts), each signature's certificates must be the same, and so must its timestamp: a
token that verifies against the signature value with the local TSA's certificate, or the
bare `GeneralizedTime` Rails embeds when the server fails. The requests the TSA received
are compared too (path, content type, user agent, authorization, request structure).
"""

import base64
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import signing
from compare import RAILS, RUST, diff
from writes import call

HOST = os.environ.get("TSA_HOST") or os.environ.get("HOOK_HOST") or "127.0.0.1"
PORT = int(os.environ.get("TSA_PORT", "4598"))
WORK = Path(tempfile.mkdtemp(prefix="parity-tsa-"))
CAPTURED = []
LOCK = threading.Lock()
FAILURES = 0


def openssl(*args, **kwargs):
    return subprocess.run(["openssl", *args], cwd=WORK, check=True, capture_output=True, **kwargs)


def make_tsa():
    """A CA, a timestamping certificate signed by it, and the `openssl ts` configuration."""
    openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", "ca.key", "-out", "ca.crt",
            "-days", "30", "-subj", "/CN=Parity TSA CA")
    openssl("req", "-newkey", "rsa:2048", "-nodes", "-keyout", "tsa.key", "-out", "tsa.csr",
            "-subj", "/CN=Parity TSA")
    (WORK / "tsa.ext").write_text("extendedKeyUsage=critical,timeStamping\nbasicConstraints=CA:FALSE\n")
    openssl("x509", "-req", "-in", "tsa.csr", "-CA", "ca.crt", "-CAkey", "ca.key", "-CAcreateserial",
            "-out", "tsa.crt", "-days", "30", "-extfile", "tsa.ext")
    (WORK / "serial").write_text("01\n")
    (WORK / "ts.cnf").write_text(
        "[ tsa ]\ndefault_tsa = tsa_config\n[ tsa_config ]\nserial = ./serial\n"
        "crypto_device = builtin\nsigner_cert = ./tsa.crt\nsigner_key = ./tsa.key\n"
        "signer_digest = sha256\ndefault_policy = 1.2.3.4.1\ndigests = sha256\n"
        "accuracy = secs:1\nordering = no\ntsa_name = no\ness_cert_id_chain = no\n"
        "ess_cert_id_alg = sha256\ncerts = ./tsa.crt\nother_policies = 1.2.3.4.2\n"
    )


class Tsa(BaseHTTPRequestHandler):
    """`/tsa` answers timestamp queries; `/fail` answers 500."""

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        with LOCK:
            CAPTURED.append({
                "path": self.path,
                "content_type": self.headers.get("Content-Type"),
                "user_agent": self.headers.get("User-Agent"),
                "authorization": self.headers.get("Authorization"),
                "query": request_shape(body),
            })
            if self.path != "/tsa":
                self.send_response(500)
                self.end_headers()
                return
            (WORK / "req.tsq").write_bytes(body)
            result = subprocess.run(
                ["openssl", "ts", "-reply", "-config", "ts.cnf", "-queryfile", "req.tsq", "-out", "resp.tsr"],
                cwd=WORK, capture_output=True,
            )
            reply = (WORK / "resp.tsr").read_bytes() if result.returncode == 0 else b""
        self.send_response(200 if reply else 500)
        self.send_header("Content-Type", "application/timestamp-reply")
        self.send_header("Content-Length", str(len(reply)))
        self.end_headers()
        self.wfile.write(reply)

    def log_message(self, *args):
        pass


def der_items(data):
    """The DER elements of `data`: (tag, content, encoding) each."""
    items, at = [], 0
    while at < len(data):
        tag, length, header = data[at], data[at + 1], 2
        if length & 0x80:
            count = length & 0x7F
            length = int.from_bytes(data[at + 2:at + 2 + count], "big")
            header += count
        items.append((tag, data[at + header:at + header + length], data[at:at + header + length]))
        at += header + length
    return items


def request_shape(body):
    """A timestamp query with its digest masked: version, algorithm, digest length, certReq."""
    try:
        (_, query, _), = der_items(body)
        parts = der_items(query)
        imprint = der_items(parts[1][1])
        return [parts[0][2].hex(), imprint[0][2].hex(), len(imprint[1][1]), [p[2].hex() for p in parts[2:]]]
    except (ValueError, IndexError):
        return ["invalid", body.hex()]


def cms_signatures(pdf):
    """Each signature (CMS) in the file whose contents are its own, with the signed data."""
    found = []
    for match in re.finditer(rb"/ByteRange\s*\[\s*(\d+)\s+(\d+)\s+(\d+)\s+(\d+)\s*\]", pdf):
        a, b, c, d = map(int, match.groups())
        contents = pdf[a + b:c].strip().strip(b"<>")
        if b == 1000000000000 or c + d > len(pdf) or not re.fullmatch(rb"[0-9a-fA-F]*", contents):
            continue
        cms = signing.der_element(bytes.fromhex(contents.decode()))
        found.append(cms)
    return found


def describe(cms):
    """A signature's certificates (SHA-256 each) and its timestamp: `token ok`/`token bad`
    for a timestamp token checked against the signature value, `time` for a bare
    `GeneralizedTime`, or None."""
    (_, info, _), = der_items(cms)
    signed_data = der_items(der_items(info)[1][1])[0][1]
    parts = der_items(signed_data)
    certificates = next(p for p in parts if p[0] == 0xA0)
    certs = [hashlib.sha256(c[2]).hexdigest()[:16] for c in der_items(certificates[1])]
    signer = der_items(der_items(parts[-1][1])[0][1])
    signature = next(p for p in signer if p[0] == 0x04)[1]
    unsigned = [p for p in signer if p[0] == 0xA1]
    stamp = None
    if unsigned:
        attribute = der_items(der_items(unsigned[0][1])[0][1])
        value = der_items(attribute[1][1])[0]
        if value[0] == 0x18:
            stamp = "time"
        else:
            (WORK / "sig.bin").write_bytes(signature)
            (WORK / "token.der").write_bytes(value[2])
            result = subprocess.run(
                ["openssl", "ts", "-verify", "-data", "sig.bin", "-in", "token.der", "-token_in",
                 "-CAfile", "ca.crt", "-untrusted", "tsa.crt"],
                cwd=WORK, capture_output=True,
            )
            stamp = "token ok" if result.returncode == 0 else "token bad"
    return {"certificates": certs, "timestamp": stamp}


def runner(script):
    result = subprocess.run(
        ["docker", "compose", "-f", "compose.rust-dev.yml", "exec", "-T", "-w", "/app", "rails",
         "bin/rails", "runner", script],
        cwd=signing.ROOT, capture_output=True, text=True, check=True,
    )
    return result.stdout.strip().splitlines()[-1] if result.stdout.strip() else ""


def get_config(key):
    """The first user's account's encrypted config value, as JSON (`null` without one)."""
    return json.loads(runner(f"""
      c = EncryptedConfig.find_by(account: User.order(:id).first.account, key: {key!r})
      puts(c ? c.value.to_json : 'null')
    """))


def set_config(key, value):
    """Stores (or with None, deletes) the first user's account's encrypted config."""
    payload = base64.b64encode(json.dumps(value).encode()).decode()
    runner(f"""
      account = User.order(:id).first.account
      value = JSON.parse(Base64.decode64({payload!r}))
      if value.nil?
        EncryptedConfig.where(account:, key: {key!r}).destroy_all
      else
        EncryptedConfig.find_or_initialize_by(account:, key: {key!r}).update!(value:)
      end
      puts 'ok'
    """)


def custom_certificate(name, legacy):
    """`EsignSettingsController#save_new_cert!` for a new PKCS#12 file: the `custom` entry
    (`OpenSSL::PKCS12.new(file, password).to_der`, URL-safe Base64)."""
    openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", f"{name}.key", "-out",
            f"{name}.crt", "-days", "30", "-subj", f"/CN=Parity {name}")
    p12 = WORK / f"{name}.p12"
    openssl("pkcs12", "-export", "-inkey", f"{name}.key", "-in", f"{name}.crt", "-certfile", "ca.crt",
            "-name", name, "-out", p12.name, "-passout", "pass:parity-secret",
            *(["-legacy"] if legacy else []))
    data = base64.b64encode(p12.read_bytes()).decode()
    return json.loads(runner(f"""
      pkcs = OpenSSL::PKCS12.new(Base64.decode64({data!r}), 'parity-secret')
      puts({{ data: Base64.urlsafe_encode64(pkcs.to_der), password: 'parity-secret',
              name: {name!r}, status: 'default' }}.to_json)
    """))


def check(label, ok, detail=""):
    global FAILURES
    FAILURES += not ok
    print(f"{'ok  ' if ok else 'FAIL'} {label}" + (f"\n     {detail}" if detail and not ok else ""))


def compare_run(label, token, template_id, fields):
    """The two-party signed submission on both backends, compared."""
    captured = {}
    ids = {}
    for name, base in (("rails", RAILS), ("rust", RUST)):
        with LOCK:
            CAPTURED.clear()
        ids[name] = signing.run_signed(base, token, template_id, fields)
        time.sleep(8)  # the completion jobs
        with LOCK:
            captured[name] = list(CAPTURED)
    a, b = signing.stored(ids["rails"]), signing.stored(ids["rust"])
    check(f"{label}: stored rows", a == b, diff(a, b))
    before = signing.FAILURES
    signing.compare_pdfs(ids["rails"], ids["rust"], signed=True)
    signing.compare_audit(ids["rails"], ids["rust"], f"{label}: audit trail")
    global FAILURES
    FAILURES += signing.FAILURES - before
    documents = {
        name: [*(d for docs in signing.result_pdfs(i) for d in docs), signing.audit_pdf(i) or b""]
        for name, i in ids.items()
    }
    details = {name: [[describe(c) for c in cms_signatures(pdf)] for pdf in pdfs]
               for name, pdfs in documents.items()}
    check(f"{label}: signature certificates and timestamps {details['rust']}",
          details["rails"] == details["rust"], f"rails {details['rails']}")
    check(f"{label}: timestamp server requests ({len(captured['rust'])})",
          captured["rails"] == captured["rust"], f"rails {captured['rails']}\n     rust  {captured['rust']}")
    return ids


def main():
    token, template_id = sys.argv[1], int(sys.argv[2])
    _, template = call(RAILS, token, "GET", f"/api/templates/{template_id}")
    fields = {f["name"]: f["uuid"] for f in template["fields"]}
    make_tsa()
    server = ThreadingHTTPServer(("0.0.0.0", PORT), Tsa)
    threading.Thread(target=server.serve_forever, daemon=True).start()

    original_url = get_config("timestamp_server_url")
    original_certs = get_config("esign_certs")
    created = []
    try:
        for label, url in (
            ("timestamp server", f"http://{HOST}:{PORT}/tsa"),
            ("timestamp server fallback", f"http://parity:secret@{HOST}:{PORT}/fail,http://{HOST}:{PORT}/tsa"),
            ("timestamp server down", f"http://{HOST}:1/tsa"),
        ):
            set_config("timestamp_server_url", url)
            created += compare_run(label, token, template_id, fields).values()
        set_config("timestamp_server_url", original_url)

        for label, legacy in (("custom certificate", False), ("custom certificate (legacy)", True)):
            entry = custom_certificate(f"parity-{'legacy' if legacy else 'aes'}", legacy)
            certs = dict(original_certs or {})
            certs["custom"] = [{**e, "status": "validate"} for e in certs.get("custom", [])] + [entry]
            set_config("esign_certs", certs)
            created += compare_run(label, token, template_id, fields).values()
    finally:
        set_config("timestamp_server_url", original_url)
        set_config("esign_certs", original_certs)
        server.shutdown()
    if not os.environ.get("KEEP"):
        for submission_id in created:
            call(RAILS, token, "DELETE", f"/api/submissions/{submission_id}")
    sys.exit(1 if FAILURES else 0)


if __name__ == "__main__":
    main()
