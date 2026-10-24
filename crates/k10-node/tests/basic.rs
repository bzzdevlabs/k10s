mod common;
use common::*;
use k10_node::demo;

#[test]
fn echo_over_http() {
    let node = start(config(2));
    let r = node.apply(bundle(vec![svc("echo", &demo::echo()), svc("path", &demo::path_echo())])).unwrap();
    assert_eq!(r.added, vec!["echo", "path"]);
    let mut c = client(&node);
    let r = c.request("GET", "/echo", b"").unwrap();
    assert_eq!((r.status, r.text().as_str()), (200, "hello\n"));
    let r = c.request("POST", "/echo/x", b"some body").unwrap();
    assert_eq!((r.status, r.text().as_str()), (200, "some body"));
    let r = c.request("GET", "/path/a/b?x=1&y=2", b"").unwrap();
    assert_eq!(r.text(), "method:0 path:/a/b?x=1&y=2");
    let r = c.request("DELETE", "/path?q", b"").unwrap();
    assert_eq!(r.text(), "method:3 path:?q");
    let r = c.request("GET", "/nope", b"").unwrap();
    assert_eq!(r.status, 404);
    let st = get(&node, "/_k10s/status");
    assert_eq!(st.status, 200);
    // The suites run on the backend named by K10S_BACKEND (default: this CPU's).
    let want = std::env::var("K10S_BACKEND").unwrap_or_else(|_| k10_node::BackendKind::native().name().into());
    assert!(st.text().contains(&format!("\"backend\":\"{want}\"")), "{}", st.text());
    node.shutdown();
}
