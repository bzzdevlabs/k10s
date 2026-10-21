#!/usr/bin/env python3
"""Prints the `Authorization` value of a signed admin request (K10s-HMAC, docs/NODE.md) for the node process at ADDR
(signed for its /_k10s/audience) with the admin key in $K10S_ADMIN_TOKEN_FILE, using bench/k10s/k10s.py's signer.

    admin_auth.py ADDR METHOD TARGET [BODY | @FILE]
    e.g. curl -H "Authorization: $(admin_auth.py 127.0.0.1:8090 GET /_k10s/status)" 127.0.0.1:8090/_k10s/status
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "bench", "k10s"))
from k10s import admin_auth  # noqa: E402

if len(sys.argv) not in (4, 5):
    sys.exit(__doc__)
data = sys.argv[4] if len(sys.argv) == 5 else ""
body = open(data[1:], "rb").read() if data.startswith("@") else data.encode()
auth = admin_auth(sys.argv[1], sys.argv[2], sys.argv[3], body)
if auth is None:
    sys.exit("admin_auth.py: set K10S_ADMIN_TOKEN_FILE to the admin key file")
print(auth)
