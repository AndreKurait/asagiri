/// Shared `/mnt/wsl/asagiri` directory across all WSL 2 distros.
pub const WSL_ASAGIRI_DIR: &str = "/mnt/wsl/asagiri";

/// Active WSL integration base directory (`ASAGIRI_WSL_DIR` override or `/mnt/wsl/asagiri`).
pub fn wsl_asagiri_dir() -> String {
    std::env::var("ASAGIRI_WSL_DIR")
        .ok()
        .map(|s| s.trim_end_matches('/').to_owned())
        .filter(|s| s.starts_with('/'))
        .unwrap_or_else(|| WSL_ASAGIRI_DIR.to_owned())
}

/// Whether `name` is a safe single-component WSL distro name.
pub fn valid_distro_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', '\0'])
        && !name.bytes().any(|b| b.is_ascii_control())
}

