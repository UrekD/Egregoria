use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output.status.success().then(|| {
        String::from_utf8(output.stdout)
            .expect("git paths and object IDs must be UTF-8")
            .trim()
            .to_owned()
    })
}

fn main() {
    // A linked worktree keeps HEAD and the branch ref in different directories.
    println!("cargo:rerun-if-changed=build.rs");
    for name in ["HEAD", "packed-refs"] {
        watch_git_path(name);
    }
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watch_git_path(&reference);
    }
    let profile = std::env::var("PROFILE").expect("Cargo build profile");
    let features = std::env::var("CARGO_CFG_FEATURE").unwrap_or_default();
    println!("cargo:rustc-env=P7_RUN_PROFILE={profile}");
    println!("cargo:rustc-env=P7_RUN_FEATURES={features}");
    let mut base_mod = String::new();
    if let Some(revision) = git(&["rev-parse", "--verify", "HEAD"]) {
        println!("cargo:rustc-env=P7_RUN_REVISION={revision}");
        for (name, path) in [
            ("P7_PROTOTYPE_TREE", "prototypes"),
            ("P7_BASE_MOD_TREE", "base_mod"),
            ("P7_ASSET_TREE", "assets"),
        ] {
            if let Some(tree) = git(&["rev-parse", &format!("{revision}:{path}")]) {
                println!("cargo:rustc-env={name}={tree}");
            }
        }
        base_mod = git(&["ls-tree", "-r", "--full-tree", &revision, "--", "base_mod"])
            .unwrap_or_default();
    }
    let out = std::env::var_os("OUT_DIR").expect("Cargo output directory");
    std::fs::write(Path::new(&out).join("p7-base-mod.git-tree"), base_mod)
        .expect("write prototype data pins");
}

fn watch_git_path(name: &str) {
    if let Some(path) = git(&["rev-parse", "--path-format=absolute", "--git-path", name]) {
        if Path::new(&path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}
