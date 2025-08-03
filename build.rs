use flatbuffers_build::BuilderOptions;

fn main() {
    println!("cargo:rerun-if-changed=src/core/fbs/blipmq.fbs");

    BuilderOptions::new_with_files(["src/core/fbs/blipmq.fbs"])
        .compile()
        .expect("Failed to compile FlatBuffers schema");
}
