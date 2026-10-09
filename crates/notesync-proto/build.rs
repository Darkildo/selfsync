//! Кодогенерация prost без системного `protoc`: схему разбирает protox (чистый Rust),
//! поэтому сборка в Docker и под WASM не требует внешних инструментов.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../proto");
    let file = proto_dir.join("notesync/v1/sync.proto");
    println!("cargo:rerun-if-changed={}", file.display());

    let fds = protox::compile([&file], [&proto_dir])?;
    prost_build::Config::new().compile_fds(fds)?;
    Ok(())
}
