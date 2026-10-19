//! Writes the demo TIR modules (`k10_node::demo`) as `.k10` files plus a manifest `app.k10s` into a directory, for
//! trying the `k10s` binary before the Ten compiler exists:
//!
//! ```text
//! cargo run -p k10-node --example demo_modules -- /tmp/k10demo
//! k10s node --data /tmp/k10data &
//! k10s apply /tmp/k10demo/app.k10s
//! curl localhost:8080/echo
//! ```
use k10_node::demo;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "k10demo".into());
    std::fs::create_dir_all(&dir).unwrap();
    let mods = [
        ("echo", demo::echo()),
        ("empty", demo::empty()),
        ("path", demo::path_echo()),
        ("kv", demo::kv()),
        ("counter", demo::counter()),
        ("memcount", demo::memcount(true)),
        ("spin", demo::spin()),
        ("burn", demo::burn()),
        ("dense", demo::dense(8)),
        ("logger", demo::logger()),
        ("chain", demo::caller("echo")),
    ];
    for (name, m) in &mods {
        std::fs::write(format!("{dir}/{name}.k10"), k10_ir::encode(m)).unwrap();
    }
    let manifest = r#"# demo services (modules built by examples/demo_modules.rs)
service echo    { code = "echo.k10"    memory = 64KiB  max_instances = 64 }
service empty   { code = "empty.k10"   memory = 4KiB }
service path    { code = "path.k10"    memory = 64KiB }
service kv      { code = "kv.k10"      memory = 64KiB }
service counter { code = "counter.k10" memory = 64KiB max_instances = 1 }
service fresh   { code = "memcount.k10" memory = 64KiB fresh = true }
service spin    { code = "spin.k10"    memory = 4KiB  cpu_ms = 100 }
service burn    { code = "burn.k10"    memory = 64KiB config = "20" }
service dense   { code = "dense.k10"   memory = 64KiB max_instances = 100000 }
service logger  { code = "logger.k10"  memory = 64KiB }
service chain   { code = "chain.k10"   memory = 64KiB }
"#;
    std::fs::write(format!("{dir}/app.k10s"), manifest).unwrap();
    println!("wrote {} modules and {dir}/app.k10s", mods.len());
}
