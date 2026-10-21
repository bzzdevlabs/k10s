//! The in-memory host API used by the tests (and by other crates through the `testhost` feature).

use super::util::*;
use crate::testhost::{ServiceConfig, TestHost, DELETE};
use k10_ir::Trap;

fn host(services: &[(&str, &str)]) -> TestHost {
    let mut h = TestHost::new();
    for (name, src) in services {
        h.add(name, compile_ok(src)).unwrap();
    }
    h
}

#[test]
fn request_basics() {
    let mut h = host(&[(
        "svc",
        r#"export fn handle() {
            let p = read_path();
            out_int(req_method()); out_str(" "); out(p, last_len()); out_str(" ");
            let b = read_body(); out(b, last_len());
            status(201);
        }"#,
    )]);
    let r = h.request("svc", DELETE, "/a/b?x=1", b"body!");
    assert_eq!((r.status, r.text().as_str(), r.trap), (201, "3 /a/b?x=1 body!", None));
    let r = h.get("svc", "");
    assert_eq!(r.text(), "0  ");
    assert_eq!(h.get("nope", "/").status, 404);
}

#[test]
fn long_paths_and_query_parameters() {
    let mut h = host(&[(
        "svc",
        r#"buffer v: [u8; 16];
        export fn handle() {
            out_int(query_int("n", -1)); out_str(",");
            out_int(query_int("m", -1)); out_str(",");
            let k = query_str("k", v, 16);
            out_int(k); out_str(":"); out(v, max(0, min(k, 16))); out_str(",");
            read_path(); out_int(last_len());
        }"#,
    )]);
    assert_eq!(h.get("svc", "/?n=12&m=-3&k=hello").text(), "12,-3,5:hello,19");
    assert_eq!(h.get("svc", "/x?m&n=1x&k=").text(), "-1,-1,0:,12");
    let long = format!("/{}?n=77", "p".repeat(3000));
    assert_eq!(h.get("svc", &long).text(), format!("77,-1,-1:,{}", long.len()));
}

#[test]
fn instance_state_init_and_fresh() {
    let src = "global n: i64 = 0; global inited: i64 = 0;
        export fn init() { inited += 1; n = 100; }
        export fn handle() { n += 1; out_int(n); out_str(\"/\"); out_int(inited); }";
    let mut h = host(&[("s", src)]);
    assert_eq!(h.get("s", "/").text(), "101/1");
    assert_eq!(h.get("s", "/").text(), "102/1");
    h.restart("s");
    assert_eq!(h.get("s", "/").text(), "101/1");
    let mut f = TestHost::new();
    f.add_with("s", compile_ok(src), ServiceConfig { fresh: true, ..Default::default() }).unwrap();
    assert_eq!(f.get("s", "/").text(), "101/1");
    assert_eq!(f.get("s", "/").text(), "101/1");
}

#[test]
fn traps_discard_the_instance() {
    let src = "global n: i64 = 0;
        export fn handle() { n += 1; if req_method() == POST { unreachable(); } out_int(n); }";
    let mut h = host(&[("s", src)]);
    assert_eq!(h.get("s", "/").text(), "1");
    assert_eq!(h.get("s", "/").text(), "2");
    let r = h.post("s", "/", b"");
    assert_eq!((r.status, r.trap), (500, Some(Trap::Unreachable)));
    assert_eq!(h.get("s", "/").text(), "1", "fresh instance after a trap");
}

#[test]
fn kv_store() {
    let src = r#"buffer v: [u8; 8];
        export fn handle() {
            let m = req_method();
            if m == POST { kv_write("k", "value", 5); kv_put_int("n", -42); }
            if m == DELETE { out_int(kv_delete("k")); out_int(kv_delete("k")); return; }
            let p = kv_read("k");
            if p == null { out_str("none"); } else { out(p, last_len()); }
            out_str(","); out_int(kv_get_int("n", 7));
            out_str(","); out_int(kv_get("k", 1, v, 2)); out(v, 2);
        }"#;
    let mut h = host(&[("a", src), ("b", src)]);
    assert_eq!(h.get("a", "/").text(), "none,7,-1\0\0");
    assert_eq!(h.post("a", "/", b"").text(), "value,-42,5va");
    assert_eq!(h.get("b", "/").text(), "none,7,-1\0\0", "namespaces are per service");
    assert_eq!(h.request("a", DELETE, "/", b"").text(), "10");
    assert_eq!(h.kv("a").unwrap().len(), 1);
    // Oversized keys trap with Host(2).
    let mut h = host(&[("s", "buffer k: [u8; 2048]; export fn handle() { kv_put(k, 2000, k, 1); }")]);
    assert_eq!(h.get("s", "/").trap, Some(Trap::Host(2)));
}

/// kv_add / kv_cas and the prelude's kv_add_int / kv_cas_bytes (all three buffer layouts it handles).
#[test]
fn kv_atomic() {
    let src = r#"buffer v: [u8; 16];
        export fn handle() {
            heap_reset();
            let m = req_method();
            if m == POST {
                out_int(kv_add_int("n", 5)); out_str(","); out_int(kv_add_int("n", -2)); out_str(",");
                out_int(kv_add_int("n", 0)); out_str(",");
                out_int(kv_add_int("absent", 0)); out_int(kv_get("absent", 6, null, 0)); out_str(",");
                // Create if absent (exp_len -1, e == 0 path), then swap with a separate new buffer (copy path).
                out_int(kv_cas_bytes("k", null, -1, "one", 3));
                out_int(kv_cas_bytes("k", null, -1, "one", 3));
                out_int(kv_cas_bytes("k", "one", 3, "two", 3));
                out_int(kv_cas_bytes("k", "one", 3, "six", 3));
                // Adjacent layout (no copy): v = "two" ++ "three".
                memcpy(v, "twothree", 8);
                out_int(kv_cas_bytes("k", v, 3, v + 3, 5));
                out_str(",");
                out(kv_read("k"), last_len()); out_str(",");
                // Compare-and-delete, raw kv_cas.
                out_int(kv_cas("k", 1, v, 5, -1)); out_int(kv_cas("k", 1, v + 3, 5, -1));
                out_int(kv_get("k", 1, null, 0));
                return;
            }
            kv_put("t", 1, "text", 4);
            kv_add_int("t", 1);
        }"#;
    let mut h = host(&[("a", src)]);
    assert_eq!(h.post("a", "/", b"").text(), "5,3,3,0-1,10101,three,01-1");
    assert_eq!(h.kv("a").unwrap().get(&b"n"[..]), Some(&3i64.to_le_bytes().to_vec()));
    // kv_add on a value that is not 8 bytes traps Host(4).
    assert_eq!(h.get("a", "/").trap, Some(Trap::Host(4)));
}

#[test]
fn calls_between_services() {
    let mut h = host(&[
        ("front", r#"export fn handle() {
            let st = call_svc("back/item/7?x=1", "ping", 4);
            out_int(st); out_str(":"); out_call_resp(); out_str(":");
            out_int(call_svc("nobody", null, 0)); out_str(":");
            out_int(call_svc("front", null, 0)); out_str(":");
            out_int(call_svc("crash", null, 0)); out_str(":"); out_int(call_resp_len());
        }"#),
        ("back", r#"export fn handle() {
            status(202);
            let p = read_path(); out(p, last_len()); out_str(" "); out_int(req_method()); out_str(" ");
            let b = read_body(); out(b, last_len());
        }"#),
        ("crash", "export fn handle() { unreachable(); }"),
    ]);
    assert_eq!(h.get("front", "/").text(), "202:/item/7?x=1 1 ping:-1:-2:-4:0");
}

#[test]
fn misc_host_functions() {
    let mut h = TestHost::new();
    let src = r#"export fn handle() {
        let c = read_config(); out(c, last_len()); out_str(",");
        let t0 = now_ns(); let t1 = now_ns(); out_int(t1 > t0); out_str(",");
        out_int(time_ms() > 1600000000000); out_str(",");
        out_int(random() != random()); out_str(",");
        log_str("hello log");
    }"#;
    h.add_with("s", compile_ok(src), ServiceConfig { config: b"cfg-bytes".to_vec(), ..Default::default() }).unwrap();
    assert_eq!(h.get("s", "/").text(), "cfg-bytes,1,1,1,");
    assert_eq!(h.logs(), &[("s".to_string(), "hello log".to_string())]);
}

#[test]
fn limits_and_range_checks() {
    // Writing beyond the memory: MemOutOfBounds.
    let mut h = host(&[("s", "export fn handle() { output_write((memsize() - 2) as *u8, 10); }")]);
    assert_eq!(h.get("s", "/").trap, Some(Trap::MemOutOfBounds));
    // Response body limit: 16 MiB → Host(1).
    let mut h = TestHost::new();
    let src = "export fn handle() { for i in 0..17 { output_write(__heap_base as *u8, 1 << 20); } }";
    h.add_with("s", compile_ok(src), ServiceConfig { mem_log2: Some(22), ..Default::default() }).unwrap();
    assert_eq!(h.get("s", "/").trap, Some(Trap::Host(1)));
}

#[test]
fn deployment_checks() {
    let mut h = TestHost::new();
    let m = compile_ok("import fn bogus() -> i64; export fn handle() { bogus(); }");
    assert!(h.add("s", m).unwrap_err().contains("unknown host function `bogus`"));
    let m = compile_ok("import fn log(p: *u8) -> i64; export fn handle() { log(null); }");
    assert!(h.add("s", m).unwrap_err().contains("takes 2 parameters"));
    let m = compile_ok("export fn other() {}");
    assert!(h.add("s", m).unwrap_err().contains("does not export `handle`"));
}
