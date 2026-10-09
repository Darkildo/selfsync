//! Кодогенерация prost без системного `protoc`: схему разбирает protox (чистый Rust),
//! поэтому сборка в Docker и под WASM не требует внешних инструментов.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Каталог крейта — из окружения во время запуска, а не `env!` при компиляции
    // скрипта: скомпилированный build-скрипт cargo переиспользует между копиями
    // workspace (worktree), и зашитый путь указывал бы на чужую копию.
    let manifest = std::env::var("CARGO_MANIFEST_DIR")?;
    let proto_dir = std::path::Path::new(&manifest).join("../../proto");
    let file = proto_dir.join("selfsync/v1/sync.proto");
    println!("cargo:rerun-if-changed={}", file.display());

    let fds = protox::compile([&file], [&proto_dir])?;
    prost_build::Config::new().compile_fds(fds)?;
    Ok(())
}
