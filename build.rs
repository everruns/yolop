use std::env;
use std::fs;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-env-changed=YOLOP_GIT_SHA");
    emit_git_rerun_paths();

    let git_sha = env::var("YOLOP_GIT_SHA")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| git_short_sha().unwrap_or_else(|| "unknown".to_string()));
    println!("cargo:rustc-env=YOLOP_GIT_SHA={git_sha}");

    let runtime_version = cargo_lock_package_version("everruns-runtime")
        .or_else(|| cargo_toml_dependency_version("everruns-runtime"))
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=YOLOP_EVERRUNS_RUNTIME_VERSION={runtime_version}");
}

fn git_short_sha() -> Option<String> {
    git_output(&["rev-parse", "--short=12", "HEAD"])
}

fn emit_git_rerun_paths() {
    if let Some(head) = git_output(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={head}");
    }
    if let Some(packed_refs) = git_output(&["rev-parse", "--git-path", "packed-refs"]) {
        println!("cargo:rerun-if-changed={packed_refs}");
    }
    if let Some(head_ref) = git_output(&["symbolic-ref", "-q", "HEAD"])
        && let Some(ref_path) = git_output(&["rev-parse", "--git-path", &head_ref])
    {
        println!("cargo:rerun-if-changed={ref_path}");
    }
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!stdout.is_empty()).then_some(stdout)
}

fn cargo_lock_package_version(package_name: &str) -> Option<String> {
    let lock = fs::read_to_string("Cargo.lock").ok()?;
    let mut in_package = false;
    let mut matched_name = false;

    for line in lock.lines().map(str::trim) {
        if line == "[[package]]" {
            in_package = true;
            matched_name = false;
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some(name) = quoted_value(line, "name") {
            matched_name = name == package_name;
            continue;
        }
        if matched_name && let Some(version) = quoted_value(line, "version") {
            return Some(version.to_string());
        }
    }
    None
}

fn cargo_toml_dependency_version(package_name: &str) -> Option<String> {
    let manifest = fs::read_to_string("Cargo.toml").ok()?;
    for line in manifest.lines().map(str::trim) {
        if !line.starts_with(package_name) {
            continue;
        }
        if let Some((_, raw_version)) = line.split_once('=') {
            let version = raw_version.trim().trim_matches('"');
            if !version.is_empty() {
                return Some(version.to_string());
            }
        }
    }
    None
}

fn quoted_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let (raw_key, raw_value) = line.split_once('=')?;
    if raw_key.trim() != key {
        return None;
    }
    raw_value.trim().strip_prefix('"')?.strip_suffix('"')
}
