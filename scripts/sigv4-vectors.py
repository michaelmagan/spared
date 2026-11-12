#!/usr/bin/env python3
"""Write the SigV4 test vectors that the remote backup client and the fake S3 in the tests check
against: crates/spared/tests/data/sigv4-vectors.json.

    python3 -m venv /tmp/v && /tmp/v/bin/pip install botocore
    /tmp/v/bin/python scripts/sigv4-vectors.py > crates/spared/tests/data/sigv4-vectors.json

Each case is signed by botocore's S3SigV4Auth (the signer the AWS CLI uses) at a fixed time, so the
file is the same on every run. The output records the botocore version. Rerun it only to add
cases; the committed file is what the tests read.
"""
import json
import sys

import botocore
import botocore.auth
from botocore.awsrequest import AWSRequest
from botocore.credentials import Credentials

EMPTY = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
AKID = "AKIAIOSFODNN7EXAMPLE"
SECRET = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"

CASES = [
    # (name, method, url, extra headers, body, region, session token, timestamp)
    ("get-object-virtual", "GET", "https://examplebucket.s3.amazonaws.com/test.txt",
     [("Range", "bytes=0-9")], b"", "us-east-1", None, "20130524T000000Z"),
    ("put-object-dollar-key", "PUT", "https://examplebucket.s3.amazonaws.com/test%24file.text",
     [("x-amz-storage-class", "REDUCED_REDUNDANCY")], b"Welcome to Amazon S3.", "us-east-1", None, "20130524T000000Z"),
    ("list-v2-path-style", "GET",
     "https://s3.eu-west-1.amazonaws.com/my-bucket?list-type=2&prefix=backups%2Fspared%2Fv1%2Fnotes%2F&continuation-token=1%2Fabc%3D%3D",
     [], b"", "eu-west-1", None, "20261112T040506Z"),
    ("put-manifest-custom-endpoint", "PUT",
     "http://127.0.0.1:9000/spared-backups/prod/spared/v1/notes/e00000001/20261112T040506Z.7f3a9c0a1b2c.1/manifest.json",
     [("Content-Type", "application/json")], b'{"format":"spared-remote-backup-v1"}', "auto", None, "20261112T040506Z"),
    ("head-with-session-token", "HEAD",
     "https://my-bucket.s3.us-west-2.amazonaws.com/a/b%20c/d~e_f-g.h",
     [], b"", "us-west-2", "FQoGZXIvYXdzEXAMPLE//token+with/odd=chars", "20261112T235959Z"),
    ("get-object-unicode-key", "GET", "https://s3.us-east-1.amazonaws.com/b-1/caf%C3%A9/%2B%26%3D",
     [], b"", "us-east-1", None, "20261231T120000Z"),
]


def sign(name, method, url, extra, body, region, token, ts):
    req = AWSRequest(method=method, url=url, data=body)
    for k, v in extra:
        req.headers[k] = v
    creds = Credentials(AKID, SECRET, token)
    auth = botocore.auth.S3SigV4Auth(creds, "s3", region)
    # The steps of SigV4Auth.add_auth, with the time fixed instead of read from the clock.
    req.context["timestamp"] = ts
    auth._modify_request_before_signing(req)
    creq = auth.canonical_request(req)
    sts = auth.string_to_sign(req, creq)
    signed = auth.signed_headers(auth.headers_to_sign(req))
    auth._inject_signature_to_request(req, auth.signature(sts, req))
    return {
        "name": name,
        "method": method,
        "url": url,
        "headers": [[k, v] for k, v in req.headers.items() if k.lower() != "authorization"],
        "payload_sha256": req.headers["X-Amz-Content-SHA256"],
        "region": region,
        "access_key_id": AKID,
        "secret_access_key": SECRET,
        "session_token": token,
        "amz_date": ts,
        "signed_headers": signed,
        "canonical_request": creq,
        "string_to_sign": sts,
        "signature": auth.signature(sts, req),
        "authorization": req.headers["Authorization"],
    }


def main():
    out = {
        "generator": f"botocore {botocore.__version__} S3SigV4Auth (scripts/sigv4-vectors.py)",
        "cases": [sign(*c) for c in CASES],
    }
    json.dump(out, sys.stdout, indent=1, sort_keys=True)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
