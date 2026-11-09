// Included by each executable package so metadata describes that package,
// rather than the independently cached astra-platform dependency.
use std::{env, path::Path, process::Command, time::SystemTime};

include!("build_metadata_inputs.rs");

fn output(command: &mut Command) -> Option<String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let result = command.output().ok()?;
    result
        .status
        .success()
        .then(|| String::from_utf8_lossy(&result.stdout).trim().to_owned())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../astra-platform/build_metadata.rs");
    println!("cargo:rerun-if-changed=../astra-platform/build_metadata_inputs.rs");
    let package = env::var("CARGO_PKG_NAME").expect("CARGO_PKG_NAME");
    let (_, inputs) = LOCAL_METADATA_INPUTS
        .iter()
        .find(|(name, _)| *name == package)
        .expect("Run python scripts/build_metadata_inputs.py for this package");
    for input in *inputs {
        println!("cargo:rerun-if-changed=../../{input}");
    }
    println!("cargo:rerun-if-changed=../../Cargo.toml");
    println!("cargo:rerun-if-changed=../../Cargo.lock");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo:rerun-if-env-changed=RUSTC");
    let timestamp = env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("system clock precedes epoch")
                .as_secs()
        });
    let compiler = output(
        Command::new(env::var_os("RUSTC").unwrap_or_else(|| "rustc".into())).arg("--version"),
    )
    .unwrap_or_else(|| "unknown".into());
    let revision =
        output(Command::new("git").args(["rev-parse", "--short=12", "HEAD"])).unwrap_or_default();
    for name in ["HEAD", "refs", "packed-refs"] {
        if let Some(path) = output(Command::new("git").args(["rev-parse", "--git-path", name])) {
            // Nonexistent paths make Cargo rerun this script on every build.
            // --git-path also resolves shared refs correctly in a worktree.
            if Path::new(&path).exists() {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
    // PROFILE collapses custom profiles to debug/release. OUT_DIR keeps their
    // actual name (.../<profile>/build/<package>/out).
    let out_dir = env::var_os("OUT_DIR").expect("OUT_DIR");
    let profile = Path::new(&out_dir)
        .ancestors()
        .nth(3)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| env::var("PROFILE").unwrap_or_default());
    for (name, value) in [
        ("ASTRA_BUILD_TIMESTAMP", timestamp.to_string()),
        ("ASTRA_BUILD_TARGET", env::var("TARGET").expect("TARGET")),
        ("ASTRA_BUILD_PROFILE", profile),
        ("ASTRA_BUILD_RUSTC", compiler),
        ("ASTRA_BUILD_REVISION", revision),
    ] {
        println!("cargo:rustc-env={name}={value}");
    }
}
