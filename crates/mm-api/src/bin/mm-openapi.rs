//! Emit the generated OpenAPI spec as YAML.
//!
//! Default: writes `contracts/api/generated/mm_api_gen.yaml` (resolved
//! relative to this crate's manifest dir, so it works from any cwd).
//! Pass an explicit output path, or `-` for stdout.
//!
//! Usage:
//!   cargo run -p mm-api --bin mm-openapi            # write the checked-in file
//!   cargo run -p mm-api --bin mm-openapi -- -       # print to stdout
//!
//! CI regenerates the file and fails on `git diff` — see the `spec-gen` job
//! in `.github/workflows/test.yml`.

use std::path::PathBuf;

fn main() {
    let yaml = mm_api::openapi::api_doc()
        .to_yaml()
        .expect("serialize OpenAPI document to YAML");

    let arg = std::env::args().nth(1);
    if arg.as_deref() == Some("-") {
        print!("{yaml}");
        return;
    }

    let out: PathBuf = match arg {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../contracts/api/generated/mm_api_gen.yaml"),
    };
    std::fs::create_dir_all(out.parent().expect("output path has a parent"))
        .expect("create output directory");
    std::fs::write(&out, yaml).expect("write generated spec");
    eprintln!("wrote {}", out.display());
}
