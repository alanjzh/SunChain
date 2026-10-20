extern crate bindgen;

use std::env;
use std::path::PathBuf;

fn gen_bindings() {
    let bindings = bindgen::Builder::default()
        .header("mulet_interface.h")
        .generate_comments(true)
        .constified_enum("")
        .rustified_enum(".*")
        // force deriving the Hash trait on basic types 
        .derive_hash(true)
        // force deriving the PratialEq trait on basic types
        .derive_partialeq(true)
        .blocklist_type("mulet_host_context")
        .allowlist_type("mulet_.*")
        .allowlist_function("mulet_.*")
        .allowlist_var("_ABI_VERSION")
        // TODO: consider removing this
        .size_t_is_usize(true)
        .generate()
        .expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("mulet_bindings.rs"))
        .expect("Couldn't write bindings!");
}

fn main() {
    gen_bindings();
}
