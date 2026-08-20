use anyhow::{Result, bail};

pub fn default_host_platform() -> Result<String> {
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        other => bail!("unsupported host OS: {other}"),
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => bail!("unsupported host architecture: {other}"),
    };

    Ok(format!("{os}/{arch}"))
}

pub fn default_linux_platform() -> Result<String> {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => bail!("unsupported host architecture: {other}"),
    };

    Ok(format!("linux/{arch}"))
}

#[cfg(test)]
mod tests {
    use super::{default_host_platform, default_linux_platform};

    #[test]
    fn resolves_linux_platform() {
        let platform = default_linux_platform().unwrap();
        assert!(platform == "linux/arm64" || platform == "linux/amd64");
    }

    #[test]
    fn resolves_host_platform() {
        let platform = default_host_platform().unwrap();
        assert!(
            platform == "linux/arm64"
                || platform == "linux/amd64"
                || platform == "darwin/arm64"
                || platform == "darwin/amd64"
        );
    }
}
