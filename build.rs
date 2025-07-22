use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=src/core/proto/message.proto");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    prost_build::Config::new()
        .out_dir(&out_dir)
        .compile_protos(&["src/core/proto/message.proto"], &["src/core/proto"])
        .expect("Failed to compile .proto");
}
