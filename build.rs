//! Make the release tag part of the build.
//!
//! The tag is the version, so a tagged build must report the tag rather than the crate
//! version. `option_env!` alone is not enough: Cargo caches the crate, and without an
//! explicit `rerun-if-env-changed` a rebuild after the variable changes would silently keep
//! the previous value.

use std::env;

/// The version the workflow handed us, normalised.
///
/// `github.ref_name` arrives as `v0.1.1`, but `--version`, the workflow's own consistency
/// check and the artifact names all quote the bare `0.1.1`. Normalising here — in the one
/// place that reads the variable — means every consumer agrees by construction instead of
/// each having to remember to strip the prefix. GitHub Actions expressions have no string
/// trimming, so the shell cannot do it for the job-level `env:`.
///
/// A leading `v` is expected; anything else is left alone rather than guessed at.
fn normalise(raw: String) -> String {
    raw.strip_prefix('v').unwrap_or(&raw).to_string()
}

fn main() {
    println!("cargo:rerun-if-env-changed=MPI_BUILD_VERSION");
    println!("cargo:rerun-if-changed=build.rs");

    if let Ok(raw) = env::var("MPI_BUILD_VERSION") {
        let version = normalise(raw);
        // Keep the exact bytes the binary will report, so the workflow's check compares
        // like with like even if the tag had surprising characters in it.
        println!("cargo:rustc-env=MPI_BUILD_VERSION={version}");
    }

    // CI verifies the binary against the tag. Saying so here puts the failure next to the
    // step that can act on it, rather than on a later run.
    let tag = env::var("GITHUB_REF_NAME").unwrap_or_default();
    let is_tag_push = env::var("GITHUB_REF_TYPE").is_ok_and(|t| t == "tag");
    if is_tag_push && !tag.is_empty() {
        let expected = normalise(tag);
        if let Some(binary) = env::var("MPI_BUILD_VERSION").ok().map(normalise) {
            if binary != expected {
                panic!("MPI_BUILD_VERSION ({binary}) 与 tag ({expected}) 不一致");
            }
        }
    }

}
