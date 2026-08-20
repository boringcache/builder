use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use oci_client::Reference;
use oci_client::secrets::RegistryAuth;

pub fn resolve_auth(reference: &Reference) -> RegistryAuth {
    if let Some(config) = load_docker_config() {
        // Try credential helper first (per-registry credHelpers, then default credsStore)
        let registry = reference.registry();
        if let Some(auth) = auth_from_cred_helper(&config, registry) {
            return auth;
        }
        // Fall back to static auths
        if let Some(auth) = auth_from_config(&config, registry) {
            return auth;
        }
    }

    RegistryAuth::Anonymous
}

pub(crate) fn docker_auth_config_json() -> Option<String> {
    if let Ok(raw) = std::env::var("DOCKER_AUTH_CONFIG") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() && parse_docker_config(trimmed).is_some() {
            return Some(trimmed.to_string());
        }
    }

    let path = docker_config_path()?;
    let data = fs::read_to_string(&path).ok()?;
    let trimmed = data.trim();
    if trimmed.is_empty() || parse_docker_config(trimmed).is_none() {
        return None;
    }
    Some(trimmed.to_string())
}

#[derive(Debug, serde::Deserialize)]
struct DockerConfig {
    #[serde(default)]
    auths: BTreeMap<String, AuthEntry>,
    #[serde(default, rename = "credsStore")]
    creds_store: Option<String>,
    #[serde(default, rename = "credHelpers")]
    cred_helpers: BTreeMap<String, String>,
}

#[derive(Debug, serde::Deserialize)]
struct AuthEntry {
    auth: Option<String>,
}

fn load_docker_config() -> Option<DockerConfig> {
    parse_docker_config(docker_auth_config_json()?.as_str())
}

fn parse_docker_config(raw: &str) -> Option<DockerConfig> {
    serde_json::from_str(raw).ok()
}

fn docker_config_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("DOCKER_CONFIG") {
        let path = PathBuf::from(dir).join("config.json");
        if path.exists() {
            return Some(path);
        }
    }

    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home).join(".docker").join("config.json");
    if path.exists() { Some(path) } else { None }
}

fn auth_from_cred_helper(config: &DockerConfig, registry: &str) -> Option<RegistryAuth> {
    let registry_variants = registry_lookup_keys(registry);

    // Check per-registry credHelpers first
    let helper = registry_variants
        .iter()
        .find_map(|key| config.cred_helpers.get(key.as_str()))
        .cloned()
        .or_else(|| config.creds_store.clone())?;

    let binary = format!("docker-credential-{helper}");
    let server_url = if registry == "docker.io" || registry == "index.docker.io" {
        "https://index.docker.io/v1/"
    } else {
        registry
    };

    let output = std::process::Command::new(&binary)
        .arg("get")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(server_url.as_bytes());
            }
            child.wait_with_output()
        })
        .ok()?;

    if !output.status.success() {
        return None;
    }

    #[derive(serde::Deserialize)]
    struct CredHelperResponse {
        #[serde(alias = "Username")]
        username: Option<String>,
        #[serde(alias = "Secret")]
        secret: Option<String>,
    }

    let resp: CredHelperResponse = serde_json::from_slice(&output.stdout).ok()?;
    let username = resp.username.filter(|u| !u.is_empty())?;
    let secret = resp.secret.filter(|s| !s.is_empty())?;

    Some(RegistryAuth::Basic(username, secret))
}

fn auth_from_config(config: &DockerConfig, registry: &str) -> Option<RegistryAuth> {
    let registry_variants = registry_lookup_keys(registry);

    for key in &registry_variants {
        if let Some(entry) = config.auths.get(key.as_str())
            && let Some(encoded) = &entry.auth
        {
            return decode_basic_auth(encoded);
        }
    }

    None
}

fn registry_lookup_keys(registry: &str) -> Vec<String> {
    let mut keys = vec![registry.to_string()];

    if registry == "docker.io" || registry == "index.docker.io" {
        keys.push("https://index.docker.io/v1/".to_string());
        keys.push("https://index.docker.io/v2/".to_string());
        keys.push("index.docker.io".to_string());
        keys.push("docker.io".to_string());
    } else {
        keys.push(format!("https://{registry}"));
    }

    keys
}

fn decode_basic_auth(encoded: &str) -> Option<RegistryAuth> {
    let decoded = base64_decode(encoded)?;
    let text = String::from_utf8(decoded).ok()?;
    let (user, pass) = text.split_once(':')?;
    Some(RegistryAuth::Basic(user.to_string(), pass.to_string()))
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let input = input.trim();
    let mut output = Vec::with_capacity(input.len() * 3 / 4);
    let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut buf = 0u32;
    let mut bits = 0u32;

    for &byte in input.as_bytes() {
        if byte == b'=' {
            break;
        }
        let val = table.iter().position(|&b| b == byte)? as u32;
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }

    Some(output)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn decodes_base64_auth() {
        let encoded = "dXNlcjpwYXNz"; // user:pass
        let decoded = base64_decode(encoded).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "user:pass");
    }

    #[test]
    fn registry_keys_include_docker_hub_variants() {
        let keys = registry_lookup_keys("docker.io");
        assert!(keys.contains(&"https://index.docker.io/v1/".to_string()));
        assert!(keys.contains(&"docker.io".to_string()));
    }

    #[test]
    fn anonymous_when_no_config() {
        let reference: Reference = "alpine:latest".parse().unwrap();
        let auth = resolve_auth(&reference);
        matches!(auth, RegistryAuth::Anonymous);
    }

    #[test]
    fn resolves_auth_from_docker_auth_config_env() {
        let previous_auth = std::env::var_os("DOCKER_AUTH_CONFIG");
        let previous_docker_config = std::env::var_os("DOCKER_CONFIG");
        let previous_home = std::env::var_os("HOME");

        unsafe {
            std::env::set_var(
                "DOCKER_AUTH_CONFIG",
                r#"{"auths":{"ghcr.io":{"auth":"dXNlcjpwYXNz"}}}"#,
            );
            std::env::remove_var("DOCKER_CONFIG");
            std::env::remove_var("HOME");
        }

        let reference: Reference = "ghcr.io/example/app:latest".parse().unwrap();
        let auth = resolve_auth(&reference);
        match auth {
            RegistryAuth::Basic(user, pass) => {
                assert_eq!(user, "user");
                assert_eq!(pass, "pass");
            }
            other => panic!("expected basic auth, got {other:?}"),
        }

        unsafe {
            match previous_auth {
                Some(value) => std::env::set_var("DOCKER_AUTH_CONFIG", value),
                None => std::env::remove_var("DOCKER_AUTH_CONFIG"),
            }
            match previous_docker_config {
                Some(value) => std::env::set_var("DOCKER_CONFIG", value),
                None => std::env::remove_var("DOCKER_CONFIG"),
            }
            match previous_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn docker_auth_config_json_falls_back_to_config_file() {
        let temp = tempdir().unwrap();
        let docker_dir = temp.path().join(".docker");
        fs::create_dir_all(&docker_dir).unwrap();
        fs::write(
            docker_dir.join("config.json"),
            r#"{"auths":{"registry.example.com":{"auth":"dXNlcjpwYXNz"}}}"#,
        )
        .unwrap();

        let previous_auth = std::env::var_os("DOCKER_AUTH_CONFIG");
        let previous_docker_config = std::env::var_os("DOCKER_CONFIG");
        let previous_home = std::env::var_os("HOME");

        unsafe {
            std::env::remove_var("DOCKER_AUTH_CONFIG");
            std::env::remove_var("DOCKER_CONFIG");
            std::env::set_var("HOME", temp.path());
        }

        let raw = docker_auth_config_json().unwrap();
        assert!(raw.contains("registry.example.com"));

        unsafe {
            match previous_auth {
                Some(value) => std::env::set_var("DOCKER_AUTH_CONFIG", value),
                None => std::env::remove_var("DOCKER_AUTH_CONFIG"),
            }
            match previous_docker_config {
                Some(value) => std::env::set_var("DOCKER_CONFIG", value),
                None => std::env::remove_var("DOCKER_CONFIG"),
            }
            match previous_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}
