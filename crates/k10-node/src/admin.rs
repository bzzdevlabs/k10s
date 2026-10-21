//! Admin API under `/_k10s/`: `GET status` (JSON), `GET metrics` (Prometheus text), `GET logs/<svc>`,
//! `POST apply` (body = encoded `k10_mesh::Bundle`), `GET health`, `GET cluster` (text, cluster mode).
//! Access control: [`gate`].

use crate::http::{Method, RequestHead};
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

/// The admin endpoint a target names (`/_k10s/logs/x?q` → `logs/x`), `None` for service traffic.
pub fn endpoint(target: &str) -> Option<&str> {
    let path = target.split('?').next().unwrap_or("");
    let rest = path.strip_prefix("/_k10s")?;
    (rest.is_empty() || rest.starts_with('/')).then(|| rest.trim_start_matches('/'))
}

/// Open endpoints: liveness and read-only counters (scrapers should not need a token that can deploy code).
fn is_public(endpoint: &str) -> bool {
    matches!(endpoint, "health" | "metrics")
}

/// Admin bearer token; only its SHA-256 is kept, compared in constant time.
#[derive(Clone)]
pub struct AdminToken([u8; 32]);

impl AdminToken {
    pub fn new(secret: &[u8]) -> AdminToken {
        AdminToken(k10_mesh::sha256::sha256(secret))
    }

    pub fn matches(&self, presented: &[u8]) -> bool {
        k10_mesh::crypto::ct_eq(&k10_mesh::sha256::sha256(presented), &self.0)
    }
}

impl std::fmt::Debug for AdminToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AdminToken(..)")
    }
}

/// Access check on a request head (body not read yet): `None` = proceed, else the status and message to close with.
pub fn gate(
    node: &NodeShared,
    admin_listener: bool,
    h: &RequestHead,
    head: &[u8],
    peer_is_loopback: impl FnOnce() -> bool,
) -> Option<(u16, &'static str)> {
    let ep = match (endpoint(&h.target), admin_listener) {
        (None, false) => return None,
        (None, true) => return Some((404, "this listener serves the admin API (/_k10s/) only")),
        (Some(ep), false) if node.admin_addr.get().is_some() => {
            return (ep != "health").then_some((404, "the admin API is served on the admin listener (--admin)"));
        }
        (Some(ep), _) => ep,
    };
    if is_public(ep) {
        return None;
    }
    match &node.cfg.admin_token {
        Some(t) => {
            let presented = h.auth.and_then(|(a, b)| bearer(&head[a..b]));
            match presented {
                Some(tok) if t.matches(tok) => None,
                _ => Some((401, "admin API: missing or invalid bearer token")),
            }
        }
        None if !h.forwarded && peer_is_loopback() => None,
        None => Some((403, "admin API: without --admin-token-file only loopback clients may use it")),
    }
}

/// The token of an `Authorization: Bearer <token>` value.
fn bearer(v: &[u8]) -> Option<&[u8]> {
    let (scheme, rest) = (v.get(..6)?, &v[6..]);
    if !scheme.eq_ignore_ascii_case(b"bearer") || !rest.first().is_some_and(|c| *c == b' ' || *c == b'\t') {
        return None;
    }
    let t = rest.trim_ascii();
    (!t.is_empty()).then_some(t)
}

pub fn handle(node: &Arc<NodeShared>, method: Method, target: &str, body: Vec<u8>) -> AdminOut {
    let path = endpoint(target).unwrap_or("");
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
    let live = s.live.load(Relaxed);
    let snap = match s.snapshot.get() {
        Some(sn) => format!("{{\"extent\":{},\"pages\":{}}}", sn.extent(), sn.pages()),
        None => "null".into(),
    };
    format!(
        concat!(
            "{{\"name\":{},\"rt\":{},\"placement\":{},\"code\":\"{}\",\"memory\":{},\"max_instances\":{},\"cpu_ms\":{},\"fresh\":{},\"may_call\":{},",
            "\"mem_reserved\":{},\"mem_used\":{},\"mem_shared\":{},",
            "\"instances\":{{\"live\":{},\"idle\":{},\"peak\":{}}},\"queue\":{},\"snapshot\":{},",
            "\"requests\":{},\"ok\":{},\"errors\":{},\"traps\":{},\"interrupted\":{},\"created\":{},\"destroyed\":{},",
            "\"reclaimed\":{},\"queued\":{},\"cpu_ms_total\":{:.3},\"init_ms_total\":{:.3},\"backoff_ms\":{},",
            "\"consecutive_traps\":{},\"kv_keys\":{},\"kv_durable\":{},\"retired\":{}}}"
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
        if s.spec.calls.is_any() {
            "\"*\"".to_string()
        } else {
            format!("[{}]", s.spec.calls.names().map(json_str).collect::<Vec<_>>().join(","))
        },
        s.mem_res.bytes(),
        live as u64 * s.instance_bytes(),
        live.saturating_sub(s.mem_res.in_use() as u32) as u64 * s.instance_bytes(),
        live,
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
        s.kv.durable(),
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
    let warnings: Vec<String> = names.iter().filter_map(|n| table.by_name[*n].call_warning()).map(|w| json_str(&w)).collect();
    let m = &node.mem;
    let kv = node.kv.stats();
    let code = node.backend.stats();
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
            "{{\"node\":{},\"version\":{},\"backend\":\"{}\",\"shards\":{},\"uptime_s\":{:.3},\"http\":\"{}\",",
            "\"code\":{{\"modules\":{},\"code_bytes\":{},\"exec_bytes\":{},\"prepared\":{},\"prepare_ms_total\":{:.3}}},",
            "\"manifest\":{{\"version\":{},\"hash\":\"{}\",\"warnings\":[{}]}},",
            "\"services\":[{}],",
            "\"memory\":{{\"rss_bytes\":{},\"slots_used\":{},\"slot_bytes\":{},\"reserved_bytes\":{},",
            "\"snapshot_mapped\":{},\"mem_limit\":{},\"mem_reserved\":{},\"mem_shared_used\":{},\"mem_shared_limit\":{},",
            "\"mem_committed\":{},\"mem_retiring\":{},\"mem_overcommit\":{},\"mem_refused\":{}}},",
            "\"http_stats\":{{\"requests\":{},\"2xx\":{},\"4xx\":{},\"5xx\":{},\"admin\":{},\"connections_open\":{},",
            "\"accepted\":{},\"rejected\":{},\"conn_timeouts\":{},\"accept_pauses\":{},\"buffered_bytes\":{},",
            "\"buffer_max\":{},\"buffer_refused\":{}}},",
            "\"exec\":{{\"invocations\":{},\"instances\":{},\"instances_created\":{},\"fibers\":{},\"fibers_busy\":{},",
            "\"backlog\":{},\"preempt_requests\":{},\"yields\":{},\"calls\":{},\"calls_failed\":{},\"timeouts\":{},",
            "\"queued_served\":{},\"cross_shard_replies\":{}}},",
            "\"kv\":{{\"namespaces\":{},\"keys\":{},\"live_bytes\":{},\"log_bytes\":{},\"flushes\":{},\"compactions\":{},",
            "\"errors\":{},\"degraded\":{}}},",
            "\"applies\":{},\"apply_errors\":{},\"reload_error\":{},\"per_shard\":[{}]{}}}"
        ),
        json_str(&node.cfg.name),
        json_str(&node.cfg.version),
        node.backend.name(),
        node.shards.len(),
        (now - node.started_ns) as f64 / 1e9,
        node.http_addr.get().map(|a| a.to_string()).unwrap_or_default(),
        code.modules,
        code.code_bytes,
        code.exec_bytes,
        code.prepared,
        code.prepare_ns as f64 / 1e6,
        table.version,
        k10_mesh::sha256::to_hex(&table.manifest_hash),
        warnings.join(","),
        services.join(","),
        sys::rss_bytes(),
        m.slots_used.load(Relaxed),
        m.slot_bytes_used.load(Relaxed),
        m.reserved_bytes.load(Relaxed),
        m.snapshot_mapped.load(Relaxed),
        node.cfg.mem_limit,
        node.budget.reserved(),
        node.budget.shared_used(),
        node.budget.limit().saturating_sub(node.budget.reserved()),
        node.budget.committed(),
        node.budget.retiring(),
        node.budget.overcommit(),
        node.budget.refused(),
        shard_sum(node, |s| &s.http_requests),
        shard_sum(node, |s| &s.http_2xx),
        shard_sum(node, |s| &s.http_4xx),
        shard_sum(node, |s| &s.http_5xx),
        shard_sum(node, |s| &s.admin_requests),
        shard_sum(node, |s| &s.conns_open),
        shard_sum(node, |s| &s.conns_accepted),
        shard_sum(node, |s| &s.conns_rejected),
        shard_sum(node, |s| &s.conn_timeouts),
        shard_sum(node, |s| &s.accept_pauses),
        node.conn_buffered.load(Relaxed),
        node.conn_buffer_max,
        shard_sum(node, |s| &s.budget_refused),
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
        kv.degraded,
        node.counters.applies.load(Relaxed),
        node.counters.apply_errors.load(Relaxed),
        node.reload_error.get().map(|e| json_str(e)).unwrap_or_else(|| "null".into()),
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
        ("k10s_service_memory_reserved_bytes", "gauge", "Node memory reserved for the service (one instance when placed here)", |s| {
            s.mem_res.bytes()
        }),
        ("k10s_service_memory_bytes", "gauge", "Memory of the service's live instances (slots)", |s| {
            s.live.load(Relaxed) as u64 * s.instance_bytes()
        }),
        ("k10s_service_kv_durable", "gauge", "1 if KV writes reach disk (0: memory-only store or degraded log)", |s| {
            s.kv.durable() as u64
        }),
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
    gauge(&mut o, "k10s_http_buffered_bytes", "gauge", "Bytes buffered in HTTP connections (request bodies, unsent responses)", node.conn_buffered.load(Relaxed));
    gauge(&mut o, "k10s_http_buffer_refused_total", "counter", "Requests refused (503) by the connection buffer budget", shard_sum(node, |s| &s.budget_refused));
    gauge(&mut o, "k10s_invocations_total", "counter", "Invocations started", shard_sum(node, |s| &s.invocations));
    gauge(&mut o, "k10s_preemptions_total", "counter", "Preemption requests by the ticker", shard_sum(node, |s| &s.preempt_requests));
    gauge(&mut o, "k10s_calls_total", "counter", "Service-to-service calls", shard_sum(node, |s| &s.calls));
    gauge(&mut o, "k10s_timeouts_total", "counter", "Calls/requests that hit their deadline", shard_sum(node, |s| &s.timeouts));
    gauge(&mut o, "k10s_fibers", "gauge", "Coroutines (pooled)", shard_sum(node, |s| &s.fibers));
    gauge(&mut o, "k10s_slots_used", "gauge", "Instance memory slots in use", node.mem.slots_used.load(Relaxed));
    gauge(&mut o, "k10s_slot_bytes", "gauge", "Address space of slots in use", node.mem.slot_bytes_used.load(Relaxed));
    let b = &node.budget;
    gauge(&mut o, "k10s_memory_limit_bytes", "gauge", "Instance memory budget (--mem)", b.limit());
    gauge(&mut o, "k10s_memory_reserved_bytes", "gauge", "Instance memory reserved for the services placed here", b.reserved());
    gauge(&mut o, "k10s_memory_shared_used_bytes", "gauge", "Instance memory used beyond the reservations (burst instances)", b.shared_used());
    gauge(&mut o, "k10s_memory_committed_bytes", "gauge", "Reservations plus burst instances", b.committed());
    gauge(&mut o, "k10s_memory_overcommit_bytes", "gauge", "Reservations beyond --mem (no burst instance possible)", b.overcommit());
    gauge(&mut o, "k10s_memory_refused_total", "counter", "Instance creations refused for lack of memory", b.refused());
    gauge(&mut o, "k10s_reload_failed", "gauge", "1 if the persisted bundle could not be reloaded at start", node.reload_error.get().is_some() as u64);
    gauge(&mut o, "k10s_rss_bytes", "gauge", "Resident set size of the node process", sys::rss_bytes());
    gauge(&mut o, "k10s_manifest_version", "gauge", "Applied manifest version", table.version);
    let code = node.backend.stats();
    gauge(&mut o, "k10s_code_modules", "gauge", "Prepared modules alive", code.modules);
    gauge(&mut o, "k10s_code_exec_bytes", "gauge", "Executable memory of prepared modules", code.exec_bytes);
    let kv = node.kv.stats();
    gauge(&mut o, "k10s_kv_keys", "gauge", "Keys in the KV store", kv.keys as u64);
    gauge(&mut o, "k10s_kv_log_bytes", "gauge", "Size of the KV logs", kv.log_bytes);
    gauge(&mut o, "k10s_kv_errors_total", "counter", "Failed KV log writes, fsyncs, rewrites and compactions", kv.errors);
    gauge(&mut o, "k10s_kv_degraded", "gauge", "KV namespaces whose log is being rewritten after an I/O error", kv.degraded as u64);
    o
}

#[cfg(test)]
mod tests {
    use super::{bearer, endpoint, json_str, AdminToken};

    #[test]
    fn endpoints_and_bearer() {
        assert_eq!(endpoint("/_k10s/status?x=1"), Some("status"));
        assert_eq!(endpoint("/_k10s//health"), Some("health"));
        assert_eq!(endpoint("/_k10s"), Some(""));
        assert_eq!(endpoint("/_k10sx/status"), None);
        assert_eq!(endpoint("/echo"), None);
        assert_eq!(bearer(b"Bearer abc"), Some(&b"abc"[..]));
        assert_eq!(bearer(b"bearer  abc "), Some(&b"abc"[..]));
        assert_eq!(bearer(b"Basic abc"), None);
        assert_eq!(bearer(b"Bearerabc"), None);
        assert_eq!(bearer(b"Bearer "), None);
        let t = AdminToken::new(b"s3cret");
        assert!(t.matches(b"s3cret") && !t.matches(b"s3cre") && !t.matches(b""));
        assert_eq!(format!("{t:?}"), "AdminToken(..)");
    }

    #[test]
    fn json_escape() {
        assert_eq!(json_str("a\"b\\c\n\u{1}"), "\"a\\\"b\\\\c\\n\\u0001\"");
    }
}
