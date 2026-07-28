#[path = "build_support.rs"]
mod build_support;

use std::env;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-env-changed=MCSDT_RELEASE_VERSION");
    let repository_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let release_override = build_support::release_override_from_environment();
    if release_override.is_none() {
        for path in build_support::git_dependency_paths(repository_root)
            .unwrap_or_else(|error| panic!("cannot declare Git version dependencies: {error}"))
        {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    let version = build_support::resolve_build_version(repository_root, release_override)
        .unwrap_or_else(|error| panic!("cannot resolve build version: {error}"));
    println!("cargo:rustc-env=MCSDT_BUILD_VERSION={}", version.as_str());
}
