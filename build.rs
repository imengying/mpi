//! Make the release tag part of the build.
//!
//! The tag is the version, so a tagged build must report the tag rather than the crate
//! version. `option_env!` alone is not enough: Cargo caches the crate, and without an
//! explicit `rerun-if-env-changed` a rebuild after the variable changes would silently keep
//! the previous value.

fn main() {
    println!("cargo:rerun-if-env-changed=MPI_BUILD_VERSION");
    println!("cargo:rerun-if-changed=build.rs");
}
