use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let def_path = manifest_dir.join("ozan-apo.def");
    println!("cargo:rerun-if-changed={}", def_path.display());
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-cdylib-link-arg=/DEF:{}", def_path.display());
    }
}
