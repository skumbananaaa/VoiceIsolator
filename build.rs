use std::{env, fs, path::PathBuf};

fn main() {
    assert_eq!(
        env::var("TARGET").unwrap(),
        "x86_64-pc-windows-msvc",
        "VoiceIsolator needs the Windows x64 MSVC target"
    );
    let lock = fs::read_to_string("cmake/Dependencies.cmake").unwrap();
    println!("cargo:rerun-if-changed=cmake/Dependencies.cmake");
    for line in lock.lines().filter_map(|line| line.strip_prefix("set(")) {
        let (key, value) = line.trim_end_matches(')').split_once(' ').unwrap();
        if key.starts_with("VOXLOCK_V2_") || key == "SHERPA_PCM_COMMIT" {
            println!("cargo:rustc-env={key}={value}");
        }
    }
    println!("cargo:rerun-if-env-changed=VOICEISOLATOR_SHERPA_SDK_DIR");
    let sdk = env::var_os("VOICEISOLATOR_SHERPA_SDK_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join(".deps/sherpa-pcm-install")
        });
    let marker = sdk.join("voiceisolator-sherpa-pin.txt");
    println!("cargo:rerun-if-changed={}", marker.display());
    let installed = fs::read_to_string(marker)
        .expect("Missing sherpa SDK pin. Run scripts/build-pcm-sdk.ps1 first.");
    assert!(
        lock.lines()
            .any(|line| line == format!("set(SHERPA_PCM_COMMIT {})", installed.trim())),
        "The sherpa SDK does not match the PCM pin"
    );
    let sdk = sdk.join("lib");
    let import = sdk.join("sherpa-onnx-c-api.lib");
    println!("cargo:rerun-if-changed={}", import.display());
    assert!(
        import.is_file(),
        "Missing sherpa SDK. Run scripts/build-pcm-sdk.ps1 first."
    );
    println!("cargo:rustc-link-search=native={}", sdk.display());
    println!("cargo:rustc-link-lib=dylib=sherpa-onnx-c-api");
    println!("cargo:rustc-link-lib=dylib=onnxruntime");
}
