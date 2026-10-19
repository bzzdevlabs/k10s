#!/usr/bin/env python3
"""analyze_probe.py <probefile> <t_event_ms>: outage stats relative to event time."""
import sys, json
rows = [tuple(map(int, l.split())) for l in open(sys.argv[1]) if l.strip()]
t0 = int(sys.argv[2])
after = [(t, c) for t, c in rows if t >= t0]
fails = [t for t, c in after if c != 200]
res = {"probes": len(rows), "failed_after_event": len(fails)}
if fails:
    first_fail, last_fail = fails[0], fails[-1]
    succ = [t for t, c in after if c == 200 and t > last_fail]
    res.update({"first_fail_ms_after_event": first_fail - t0, "last_fail_ms_after_event": last_fail - t0,
                "first_success_after_last_fail_ms": (succ[0] - t0) if succ else None,
                "outage_window_ms": last_fail - first_fail})
else:
    res.update({"outage_window_ms": 0})
print(json.dumps(res))
