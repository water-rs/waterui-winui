//! Regenerates `src/bindings.rs` for `waterui-winui` — the same generation
//! `build.rs` runs into `OUT_DIR`, pulled in from `build/bindgen.rs` so one
//! implementation serves both. `src/bindings.rs` and `winmd/` are gitignored
//! build artifacts now (#93); this tool only exists to diff regenerated
//! output and to inspect the metadata with `query`.
//!
//! Usage:
//!   cargo run -p winui-bindgen           fetch metadata + write src/bindings.rs
//!   cargo run -p winui-bindgen -- fetch  only refresh the `winmd/` cache

use std::path::PathBuf;
use std::process::ExitCode;

#[path = "../../../build/bindgen.rs"]
mod bindgen;

const WINMD_DIR: &str = "winmd";
const EXTRAS_RDL: &str = "build/extras.rdl";
const EXTRAS_WINMD: &str = "winmd/extras.winmd";
const RUNTIME_FILTER: &str = "build/runtime.txt";
const CONTROLS_FILTER: &str = "build/controls.txt";
const OUTPUT_FILE: &str = "src/bindings.rs";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("tools/bindgen must live two levels below the workspace root")
        .to_path_buf()
}

fn main() -> ExitCode {
    let root = workspace_root();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("query") {
        return query(&root, &args[1..]);
    }

    let fetch_only = args.iter().any(|arg| arg == "fetch");
    let winmd_dir = root.join(WINMD_DIR);
    if let Err(error) =
        bindgen::download_nupkg().and_then(|nupkg| bindgen::extract_winmds(&nupkg, &winmd_dir))
    {
        eprintln!("fetch failed: {error}");
        return ExitCode::FAILURE;
    }
    if fetch_only {
        return ExitCode::SUCCESS;
    }

    if let Err(error) = bindgen::compile_extras(&root.join(EXTRAS_RDL), &root.join(EXTRAS_WINMD)) {
        eprintln!("extras winmd failed: {error}");
        return ExitCode::FAILURE;
    }
    let output = root.join(OUTPUT_FILE);
    bindgen::generate(
        &winmd_dir,
        &[root.join(RUNTIME_FILTER), root.join(CONTROLS_FILTER)],
        &output,
    );
    println!("wrote {}", output.display());
    ExitCode::SUCCESS
}

/// Prints the namespace and method names of every type matching `names`.
///
/// Usage: `cargo run -p winui-bindgen -- query HoldingState PointerPoint`
fn query(root: &std::path::Path, names: &[String]) -> ExitCode {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(root.join(WINMD_DIR)).expect("winmd dir — run `fetch` first") {
        let path = entry.expect("winmd entry").path();
        if path.extension().is_some_and(|ext| ext == "winmd")
            && let Some(file) = windows_metadata::reader::File::read(&path)
        {
            files.push(file);
        }
    }
    files.push(
        windows_metadata::reader::File::new(windows_default::WINRT.to_vec())
            .expect("windows-default winmd"),
    );
    let index = windows_metadata::reader::Index::new(files);
    for name in names {
        let mut found = false;
        for (namespace, type_name, def) in index.iter() {
            if type_name != name.as_str() {
                continue;
            }
            found = true;
            println!("{namespace}.{name}");
            for method in def.methods() {
                println!("  .{}", method.name());
            }
        }
        if !found {
            println!("{name}: not found");
        }
    }
    ExitCode::SUCCESS
}
