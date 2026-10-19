#!/usr/bin/env python3
"""probe.py <port> <outfile> <duration_s> [interval_ms]: fresh-connection GET every interval; logs 'epoch_ms code' (0 = failure)."""
import http.client, sys, time
port, out, dur = int(sys.argv[1]), sys.argv[2], float(sys.argv[3])
iv = float(sys.argv[4]) / 1000 if len(sys.argv) > 4 else 0.02
end = time.time() + dur
with open(out, "w", buffering=1) as f:
    while time.time() < end:
        t = time.time(); code = 0
        try:
            c = http.client.HTTPConnection("127.0.0.1", port, timeout=0.5)
            c.request("GET", "/"); r = c.getresponse(); r.read(); code = r.status; c.close()
        except Exception:
            pass
        f.write(f"{int(t*1000)} {code}\n")
        s = iv - (time.time() - t)
        if s > 0: time.sleep(s)
