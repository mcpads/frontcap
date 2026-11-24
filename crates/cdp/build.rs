fn main() {
    let src = "protocol/protocol.json";
    println!("cargo:rerun-if-changed={src}");
    let json = std::fs::read_to_string(src).expect("read protocol.json");
    let code = cdp_codegen::generate(&json).expect("generate protocol");
    let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("protocol.rs");
    std::fs::write(out, code).expect("write protocol.rs");
}
