//! `K10_JIT_DUMP=dir` writes each compiled module's code blob and a function map (own test binary: the variable
//! is read once per process).

mod common;

#[test]
fn dump_writes_code_and_map() {
    let dir = std::env::temp_dir().join(format!("k10jit-dump-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::env::set_var("K10_JIT_DUMP", &dir);
    let mut mb = k10_ir::ModuleBuilder::new();
    let f = mb.func(1, 1, |f| {
        f.local_get(0).const_(1).add();
    });
    mb.export("inc", f);
    let m = mb.build();
    let jit = common::compile(&m).unwrap();
    let blob = common::compile_blob(&m, &k10_jit::CompileOptions::default()).unwrap();
    let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    files.sort();
    assert_eq!(files.len(), 2, "{files:?}");
    let bin = std::fs::read(&files[0]).unwrap();
    assert_eq!(bin.len(), jit.code_bytes());
    assert_eq!(&bin[..], blob.code(), "dump is the unpatched blob");
    let map = std::fs::read_to_string(&files[1]).unwrap();
    assert!(map.contains("inc") && map.contains("entry"), "{map}");
    std::fs::remove_dir_all(&dir).unwrap();
}
