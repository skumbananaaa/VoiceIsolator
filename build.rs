use std::{env, fs, path::PathBuf};

use image::{
    ExtendedColorType, RgbaImage,
    codecs::ico::{IcoEncoder, IcoFrame},
    imageops::{self, FilterType},
};

#[path = "rust/brand.rs"]
mod brand;

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
    if env::var_os("CARGO_FEATURE_DESKTOP").is_some() {
        embed_branding();
    }
}

fn embed_branding() {
    println!("cargo:rerun-if-changed=rust/brand.rs");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"));
    let icon = out.join("voiceisolator.ico");
    let image = RgbaImage::from_raw(brand::SIZE, brand::SIZE, brand::waveform(brand::STANDBY))
        .expect("valid waveform image");
    let frames = [16, 24, 32, 48, 64, 128, 256].map(|size| {
        let resized = imageops::resize(&image, size, size, FilterType::Triangle);
        IcoFrame::as_png(resized.as_raw(), size, size, ExtendedColorType::Rgba8)
            .expect("encode waveform icon frame")
    });
    IcoEncoder::new(fs::File::create(&icon).expect("create waveform icon"))
        .encode_images(&frames)
        .expect("write waveform icon");

    let version = env!("CARGO_PKG_VERSION");
    let numeric = concat!(
        env!("CARGO_PKG_VERSION_MAJOR"),
        ",",
        env!("CARGO_PKG_VERSION_MINOR"),
        ",",
        env!("CARGO_PKG_VERSION_PATCH"),
        ",0"
    );
    let icon_path = icon
        .to_str()
        .expect("Unicode waveform icon path")
        .replace('\\', "/");
    for (target, description) in [
        ("voiceisolator_live", "VoiceIsolator"),
        ("voiceisolator_engine", "VoiceIsolator Audio Engine"),
    ] {
        let resource = out.join(format!("{target}.rc"));
        fs::write(
            &resource,
            format!(
                r#"#pragma code_page(65001)
1 ICON "{icon_path}"
1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEFLAGSMASK 0x3fL
FILEFLAGS 0
FILEOS 0x40004L
FILETYPE 1
FILESUBTYPE 0
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "FileDescription", "{description}\0"
            VALUE "FileVersion", "{version}\0"
            VALUE "InternalName", "{target}\0"
            VALUE "OriginalFilename", "{target}.exe\0"
            VALUE "ProductName", "VoiceIsolator\0"
            VALUE "ProductVersion", "{version}\0"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x0409, 1200
    END
END
"#
            ),
        )
        .expect("write Windows branding resource");
        embed_resource::compile_for(&resource, [target], embed_resource::NONE)
            .manifest_required()
            .expect("embed Windows branding resource");
    }
}
