use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const ICON_SIZES: [u32; 4] = [16, 32, 48, 256];

fn main() {
    println!("cargo:rerun-if-changed=assets/geonames/cities1000.sqlite");
    println!("cargo:rerun-if-changed=assets/icon.png");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    println!("cargo:rerun-if-env-changed=EXIFMETA_VERSION_PRERELEASE");
    println!("cargo:rerun-if-env-changed=EXIFMETA_VERSION_METADATA");
    println!("cargo:rerun-if-env-changed=EXIFMETA_SUPPRESS_GIT_DIRTY");

    if !Path::new("assets/geonames/cities1000.sqlite").is_file() {
        panic!(
            "missing assets/geonames/cities1000.sqlite; run `python tools/geonames_to_sqlite.py` to generate it"
        );
    }

    emit_env("EXIFMETA_BUILD_DATE", &build_date());
    emit_env(
        "EXIFMETA_GIT_COMMIT",
        &git(["rev-parse", "--short=7", "HEAD"]),
    );
    emit_env(
        "EXIFMETA_GIT_BRANCH",
        &git(["rev-parse", "--abbrev-ref", "HEAD"]),
    );
    emit_env(
        "EXIFMETA_GIT_DIRTY",
        if should_emit_git_dirty() { "true" } else { "" },
    );
    emit_env(
        "EXIFMETA_VERSION_PRERELEASE",
        &std::env::var("EXIFMETA_VERSION_PRERELEASE").unwrap_or_default(),
    );
    emit_env(
        "EXIFMETA_VERSION_METADATA",
        &std::env::var("EXIFMETA_VERSION_METADATA").unwrap_or_default(),
    );

    embed_windows_icon();
}

fn emit_env(key: &str, value: &str) {
    println!("cargo:rustc-env={key}={value}");
}

fn embed_windows_icon() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let icon_path = generate_windows_icon().expect("failed to generate Windows icon");
    let icon_path = icon_path
        .to_str()
        .expect("generated Windows icon path is not valid UTF-8");

    let mut resource = winresource::WindowsResource::new();
    resource.set_icon(icon_path);
    resource
        .compile()
        .expect("failed to embed Windows icon resource");
}

fn generate_windows_icon() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let source_icon = image::ImageReader::open("assets/icon.png")?
        .decode()?
        .to_rgba8();
    let mut icon_dir = ico::IconDir::new(ico::ResourceType::Icon);

    for size in ICON_SIZES {
        let resized = image::imageops::resize(
            &source_icon,
            size,
            size,
            image::imageops::FilterType::Lanczos3,
        );
        let icon_image = ico::IconImage::from_rgba_data(size, size, resized.into_raw());
        icon_dir.add_entry(ico::IconDirEntry::encode(&icon_image)?);
    }

    let output_path = PathBuf::from(std::env::var("OUT_DIR")?).join("exifmeta.ico");
    let output_file = File::create(&output_path)?;
    icon_dir.write(output_file)?;

    Ok(output_path)
}

fn build_date() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_default()
}

fn git<const N: usize>(args: [&str; N]) -> String {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default()
}

fn is_git_dirty() -> bool {
    Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| !output.stdout.is_empty())
        .unwrap_or(false)
}

fn should_emit_git_dirty() -> bool {
    std::env::var("EXIFMETA_SUPPRESS_GIT_DIRTY").as_deref() != Ok("true") && is_git_dirty()
}
