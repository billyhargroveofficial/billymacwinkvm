use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads/main");
    println!("cargo:rerun-if-env-changed=SOFTKVM_BUILD_GIT_HASH");

    let git_hash = std::env::var("SOFTKVM_BUILD_GIT_HASH")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            Command::new("git")
                .args(["rev-parse", "--short", "HEAD"])
                .output()
                .ok()
                .and_then(|output| {
                    output
                        .status
                        .success()
                        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
                })
        })
        .unwrap_or_else(|| "unknown".to_owned());

    println!("cargo:rustc-env=SOFTKVM_BUILD_GIT_HASH={git_hash}");
}
