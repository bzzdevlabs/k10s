//! Embeds the k10s build version (scripts/buildinfo.rs).
include!("../../scripts/buildinfo.rs");

fn main() {
    buildinfo::emit();
}
