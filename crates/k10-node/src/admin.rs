//! Admin API under `/_k10s/`: `GET status` (JSON), `GET metrics` (Prometheus text), `GET logs/<svc>`,
//! `POST apply` (body = encoded `k10_mesh::Bundle`), `GET health`, `GET cluster` (text, cluster mode).

use crate::http::Method;
use crate::node::{short_hash, NodeShared};
use crate::service::ServiceRt;
use crate::sys;
use k10_mesh::Bundle;
use std::fmt::Write as _;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

pub enum AdminOut {
    Respond { status: u16, ctype: &'static str, body: Vec<u8> },
    /// Apply runs on a helper thread (preparing modules can take a while); the shard answers when it is done.
    Apply(Vec<u8>),
}

fn respond(status: u16, ctype: &'static str, body: impl Into<Vec<u8>>) -> AdminOut {
    AdminOut::Respond { status, ctype, body: body.into() }
}

pub fn handle(node: &Arc<NodeShared>, method: Method, target: &str, body: Vec<u8>) -> AdminOut {
    let path = target.split('?').next().unwrap_or("");
    let path = path.strip_prefix("/_k10s").unwrap_or(path).trim_start_matches('/');
    match (method, path) {
        (Method::Get | Method::Head, "status") => respond(200, "application/json", status_json(node)),
        (Method::Get | Method::Head, "metrics") => respond(200, "text/plain; version=0.0.4", metrics(node)),
        (Method::Get | Method::Head, "health") => respond(200, "text/plain", "ok\n"),
        (Method::Get | Method::Head, "cluster") => match node.cluster_hooks() {
            Some(c) => respond(200, "text/plain; charset=utf-8", c.status_text()),
            None => respond(404, "text/plain", "not in cluster mode\n"),
        },
        (Method::Post | Method::Put, "apply") => AdminOut::Apply(body),
        (Method::Get | Method::Head, p) if p.starts_with("logs/") => {
            let svc = &p[5..];
            match node.logs.get(svc) {
                Some(r) => respond(200, "text/plain; charset=utf-8", r.render()),
                None if node.current_table().get(svc).is_some() => respond(200, "text/plain; charset=utf-8", ""),
                None => respond(404, "text/plain", format!("unknown service `{svc}`\n")),
            }
        }
        (_, "status" | "metrics" | "health" | "apply" | "cluster") => respond(405, "text/plain", "method not allowed\n"),
        (_, p) if p.starts_with("logs/") => respond(405, "text/plain", "method not allowed\n"),
        _ => respond(404, "text/plain", "unknown admin endpoint\n"),
    }
}

/// Decodes and applies a bundle; returns the HTTP status and JSON body.
pub fn apply_bytes(node: &NodeShared, bytes: &[u8]) -> (u16, Vec<u8>) {
    if let Some(c) = node.cluster_hooks() {
        let (st, body) = c.apply(bytes);
        if st != 200 {
            node.counters.apply_errors.fetch_add(1, Relaxed);
        }
        return (st, body);
    }
    let r = Bundle::decode(bytes).map_err(|e| (400, format!("invalid bundle: {e}"))).and_then(|b| {
        node.apply(b, true).map_err(|e| (if e.starts_with("stale") { 409 } else { 422 }, e))
    });
    match r {
        Ok(rep) => (200, rep.to_json().into_bytes()),
        Err((st, e)) => {
            node.counters.apply_errors.fetch_add(1, Relaxed);
            (st, format!("{{\"ok\":false,\"error\":{}}}", json_str(&e)).into_bytes())
        }
    }
}

pub fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn svc_json(s: &ServiceRt, now: u64) -> String {
    let snap = match s.snapshot.get() {
        Some(sn) => format!("{{\"extent\":{},\"pages\":{}}}", sn.extent(), sn.pages()),
        None => "null".into(),
    };
    format!(
        concat!(
            "{{\"name\":{},\"rt\":{},\"placement\":{},\"code\":\"{}\",\"memory\":{},\"max_instances\":{},\"cpu_ms\":{},\"fresh\":{},",
            "\"instances\":{{\"live\":{},\"idle\":{},\"peak\":{}}},\"queue\":{},\"snapshot\":{},",
            "\"requests\":{},\"ok\":{},\"errors\":{},\"traps\":{},\"interrupted\":{},\"created\":{},\"destroyed\":{},",
            "\"reclaimed\":{},\"queued\":{},\"cpu_ms_total\":{:.3},\"init_ms_total\":{:.3},\"backoff_ms\":{},",
            "\"consecutive_traps\":{},\"kv_keys\":{},\"retired\":{}}}"
        ),
        json_str(s.name()),
        s.id,
        match &s.placement {
            crate::service::Placement::Local => "\"local\"".to_string(),
            crate::service::Placement::Remote { nodes } => {
                let ids: Vec<String> = nodes.iter().map(|n| format!("\"{n}\"")).collect();
                format!("[{}]", ids.join(","))
            }
        },
        short_hash(&s.spec.code_hash),
        s.spec.mem_bytes(),
        s.spec.max_instances,
        s.spec.cpu_ms,
        s.spec.fresh,
        s.live.load(Relaxed),
        s.idle_total(),
        s.peak.load(Relaxed),
        s.waiting.load(Relaxed),
        snap,
        s.sum(|x| &x.requests),
        s.sum(|x| &x.ok),
        s.sum(|x| &x.errors),
        s.sum(|x| &x.traps),
        s.sum(|x| &x.interrupted),
        s.sum(|x| &x.created),
        s.sum(|x| &x.destroyed),
        s.sum(|x| &x.reclaimed),
        s.sum(|x| &x.queued),
        s.sum(|x| &x.cpu_ns) as f64 / 1e6,
        s.sum(|x| &x.init_ns) as f64 / 1e6,
        s.backoff_remaining_ns(now) / 1_000_000,
        s.consecutive_traps(),
        s.kv.len(),
        s.retired.load(Relaxed),
    )
}

/// Sum of a per-shard counter.
fn shard_sum(node: &NodeShared, f: impl Fn(&crate::shard::ShardStats) -> &std::sync::atomic::AtomicU64) -> u64 {
    node.shards.iter().map(|s| f(&s.stats).load(Relaxed)).sum()
}

pub fn status_json(node: &NodeShared) -> String {
    let now = sys::mono_ns();
    let table = node.current_table();
    let mut names: Vec<&String> = table.by_name.keys().collect();
    names.sort();
    let services: Vec<String> = names.iter().map(|n| svc_json(&table.by_name[*n], now)).collect();
    let m = &node.mem;
    let kv = node.kv.stats();
    let per_shard: Vec<String> = node
        .shards
        .iter()
        .map(|s| {
            format!(
                "{{\"requests\":{},\"connections\":{},\"instances\":{},\"fibers\":{},\"busy\":{}}}",
                s.stats.http_requests.load(Relaxed),
                s.stats.conns_open.load(Relaxed),
                s.stats.instances.load(Relaxed),
                s.stats.fibers.load(Relaxed),
                s.stats.fibers_busy.load(Relaxed)
            )
        })
        .collect();
    format!(
        concat!(
            "{{\"node\":{},\"version\":\"{}\",\"backend\":\"{}\",\"shards\":{},\"uptime_s\":{:.3},\"http\":\"{}\",",
            "\"manifest\":{{\"version\":{},\"hash\":\"{}\"}},",
            "\"services\":[{}],",
            "\"memory\":{{\"rss_bytes\":{},\"slots_used\":{},\"slot_bytes\":{},\"reserved_bytes\":{},",
            "\"snapshot_mapped\":{},\"mem_limit\":{}}},",
            "\"http_stats\":{{\"requests\":{},\"2xx\":{},\"4xx\":{},\"5xx\":{},\"admin\":{},\"connections_open\":{},",
            "\"accepted\":{},\"rejected\":{},\"conn_timeouts\":{}}},",
            "\"exec\":{{\"invocations\":{},\"instances\":{},\"instances_created\":{},\"fibers\":{},\"fibers_busy\":{},",
            "\"backlog\":{},\"preempt_requests\":{},\"yields\":{},\"calls\":{},\"calls_failed\":{},\"timeouts\":{},",
            "\"queued_served\":{},\"cross_shard_replies\":{}}},",
            "\"kv\":{{\"namespaces\":{},\"keys\":{},\"live_bytes\":{},\"log_bytes\":{},\"flushes\":{},\"compactions\":{},",
            "\"errors\":{}}},",
            "\"applies\":{},\"apply_errors\":{},\"per_shard\":[{}]{}}}"
        ),
        json_str(&node.cfg.name),
        env!("CARGO_PKG_VERSION"),
        node.backend.name(),
        node.shards.len(),
        (now - node.started_ns) as f64 / 1e9,
        node.http_addr.get().map(|a| a.to_string()).unwrap_or_default(),
        table.version,
        k10_mesh::sha256::to_hex(&table.manifest_hash),
        services.join(","),
        sys::rss_bytes(),
        m.slots_used.load(Relaxed),
        m.slot_bytes_used.load(Relaxed),
        m.reserved_bytes.load(Relaxed),
        m.snapshot_mapped.load(Relaxed),
        node.cfg.mem_limit,
        shard_sum(node, |s| &s.http_requests),
        shard_sum(node, |s| &s.http_2xx),
        shard_sum(node, |s| &s.http_4xx),
        shard_sum(node, |s| &s.http_5xx),
        shard_sum(node, |s| &s.admin_requests),
        shard_sum(node, |s| &s.conns_open),
        shard_sum(node, |s| &s.conns_accepted),
        shard_sum(node, |s| &s.conns_rejected),
        shard_sum(node, |s| &s.conn_timeouts),
        shard_sum(node, |s| &s.invocations),
        shard_sum(node, |s| &s.instances),
        shard_sum(node, |s| &s.instances_created),
        shard_sum(node, |s| &s.fibers),
        shard_sum(node, |s| &s.fibers_busy),
        shard_sum(node, |s| &s.backlog),
        shard_sum(node, |s| &s.preempt_requests),
        shard_sum(node, |s| &s.yields),
        shard_sum(node, |s| &s.calls),
        shard_sum(node, |s| &s.calls_failed),
        shard_sum(node, |s| &s.timeouts),
        shard_sum(node, |s| &s.queued_served),
        shard_sum(node, |s| &s.cross_shard_replies),
        kv.namespaces,
        kv.keys,
        kv.live_bytes,
        kv.log_bytes,
        kv.flushes,
        kv.compactions,
        kv.errors,
        node.counters.applies.load(Relaxed),
        node.counters.apply_errors.load(Relaxed),
        per_shard.join(","),
        node.cluster_hooks().map(|c| format!(",\"cluster\":{}", c.status_json())).unwrap_or_default(),
    )
}

fn prom_label(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

pub fn metrics(node: &NodeShared) -> String {
    let mut o = String::new();
    let table = node.current_table();
    let mut svcs: Vec<&Arc<ServiceRt>> = table.by_name.values().collect();
    svcs.sort_by(|a, b| a.name().cmp(b.name()));
    type Get = fn(&ServiceRt) -> u64;
    let per_svc: &[(&str, &str, &str, Get)] = &[
        ("k10s_service_requests_total", "counter", "Requests dispatched to the service", |s| s.sum(|x| &x.requests)),
        ("k10s_service_ok_total", "counter", "Invocations that returned a 2xx/3xx status", |s| s.sum(|x| &x.ok)),
        ("k10s_service_errors_total", "counter", "Invocations that failed or returned >= 400", |s| s.sum(|x| &x.errors)),
        ("k10s_service_traps_total", "counter", "Invocations aborted by a trap", |s| s.sum(|x| &x.traps)),
        ("k10s_service_cpu_interrupts_total", "counter", "Invocations aborted for exceeding cpu_ms", |s| s.sum(|x| &x.interrupted)),
        ("k10s_service_instances_created_total", "counter", "Instances created", |s| s.sum(|x| &x.created)),
        ("k10s_service_instances_reclaimed_total", "counter", "Idle instances reclaimed", |s| s.sum(|x| &x.reclaimed)),
        ("k10s_service_cpu_seconds_total", "counter", "CPU time of invocations (ns)", |s| s.sum(|x| &x.cpu_ns)),
        ("k10s_service_instances", "gauge", "Live instances", |s| s.live.load(Relaxed) as u64),
        ("k10s_service_instances_idle", "gauge", "Idle instances", |s| s.idle_total() as u64),
        ("k10s_service_queue", "gauge", "Requests waiting for an instance", |s| s.waiting.load(Relaxed) as u64),
    ];
    for (name, ty, help, get) in per_svc {
        let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} {ty}");
        for s in &svcs {
            let v = get(s);
            if *name == "k10s_service_cpu_seconds_total" {
                let _ = writeln!(o, "{name}{{service=\"{}\"}} {:.6}", prom_label(s.name()), v as f64 / 1e9);
            } else {
                let _ = writeln!(o, "{name}{{service=\"{}\"}} {v}", prom_label(s.name()));
            }
        }
    }
    let gauge = |o: &mut String, name: &str, ty: &str, help: &str, v: u64| {
        let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} {ty}\n{name} {v}");
    };
    let _ = writeln!(o, "# HELP k10s_http_responses_total HTTP responses by status class\n# TYPE k10s_http_responses_total counter");
    for (class, f) in [
        ("2xx", shard_sum(node, |s| &s.http_2xx)),
        ("4xx", shard_sum(node, |s| &s.http_4xx)),
        ("5xx", shard_sum(node, |s| &s.http_5xx)),
    ] {
        let _ = writeln!(o, "k10s_http_responses_total{{class=\"{class}\"}} {f}");
    }
    gauge(&mut o, "k10s_http_connections", "gauge", "Open HTTP connections", shard_sum(node, |s| &s.conns_open));
    gauge(&mut o, "k10s_invocations_total", "counter", "Invocations started", shard_sum(node, |s| &s.invocations));
    gauge(&mut o, "k10s_preemptions_total", "counter", "Preemption requests by the ticker", shard_sum(node, |s| &s.preempt_requests));
    gauge(&mut o, "k10s_calls_total", "counter", "Service-to-service calls", shard_sum(node, |s| &s.calls));
    gauge(&mut o, "k10s_timeouts_total", "counter", "Calls/requests that hit their deadline", shard_sum(node, |s| &s.timeouts));
    gauge(&mut o, "k10s_fibers", "gauge", "Coroutines (pooled)", shard_sum(node, |s| &s.fibers));
    gauge(&mut o, "k10s_slots_used", "gauge", "Instance memory slots in use", node.mem.slots_used.load(Relaxed));
    gauge(&mut o, "k10s_slot_bytes", "gauge", "Address space of slots in use", node.mem.slot_bytes_used.load(Relaxed));
    gauge(&mut o, "k10s_rss_bytes", "gauge", "Resident set size of the node process", sys::rss_bytes());
    gauge(&mut o, "k10s_manifest_version", "gauge", "Applied manifest version", table.version);
    let kv = node.kv.stats();
    gauge(&mut o, "k10s_kv_keys", "gauge", "Keys in the KV store", kv.keys as u64);
    gauge(&mut o, "k10s_kv_log_bytes", "gauge", "Size of the KV logs", kv.log_bytes);
    o
}

#[cfg(test)]
mod tests {
    use super::json_str;

    #[test]
    fn json_escape() {
        assert_eq!(json_str("a\"b\\c\n\u{1}"), "\"a\\\"b\\\\c\\n\\u0001\"");
    }
}
