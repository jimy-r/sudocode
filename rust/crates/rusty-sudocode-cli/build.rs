use std::env;
use std::path::PathBuf;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .current_dir(env::var_os("CARGO_MANIFEST_DIR")?)
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout).ok()
            } else {
                None
            }
        })
        .map(|value| value.trim().to_string())
}

fn watch_git_path(name: &str) {
    let Some(path) = git(&["rev-parse", "--path-format=absolute", "--git-path", name]) else {
        return;
    };
    let mut path = PathBuf::from(path);
    // Packed branch refs may have no loose file yet. Watch their existing
    // parent so the next commit's loose ref creation also invalidates metadata.
    while !path.exists() {
        if !path.pop() {
            return;
        }
    }
    println!("cargo:rerun-if-changed={}", path.display());
}

fn main() {
    let git_sha = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=GIT_SHA={git_sha}");

    // TARGET is always set by Cargo during build
    let target = env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=TARGET={target}");

    // Build date from SOURCE_DATE_EPOCH (reproducible builds) or current UTC date.
    // Intentionally ignoring time component to keep output deterministic within a day.
    let build_date = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|epoch| epoch.parse::<i64>().ok())
        .map(|_ts| {
            // Use SOURCE_DATE_EPOCH to derive date via chrono if available;
            // for simplicity we just use the env var as a signal and fall back
            // to build-time env. In practice CI sets this via workflow.
            std::env::var("BUILD_DATE").unwrap_or_else(|_| "unknown".to_string())
        })
        .or_else(|| std::env::var("BUILD_DATE").ok())
        .unwrap_or_else(|| {
            // Fall back to current date via `date` command
            Command::new("date")
                .args(["+%Y-%m-%d"])
                .output()
                .ok()
                .and_then(|o| {
                    if o.status.success() {
                        String::from_utf8(o.stdout).ok()
                    } else {
                        None
                    }
                })
                .map_or_else(|| "unknown".to_string(), |s| s.trim().to_string())
        });
    println!("cargo:rustc-env=BUILD_DATE={build_date}");

    watch_git_path("HEAD");
    if let Some(reference) = git(&["symbolic-ref", "--quiet", "HEAD"]) {
        watch_git_path(&reference);
        let packed = git(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "packed-refs",
        ]);
        if let Some(path) = packed.filter(|path| PathBuf::from(path).is_file()) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo:rerun-if-env-changed=BUILD_DATE");
}
