pub mod archive;
pub mod docker;
pub mod oci;
pub mod tar;

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs::{self, File};
    use std::io::Read;
    use std::path::{Path, PathBuf};

    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use super::oci::{OCI_GZIP_LAYER_MEDIA_TYPE, export_pipeline_oci};
    use super::tar::{export_pipeline_tar_from_rootfs, export_pipeline_tar_zst_from_rootfs};
    use crate::schema::{
        ExportConfig, ExportFormat, ImageMetadata, Input, Operation, Pipeline, Step,
    };

    #[test]
    fn artifact_exports_are_readable_across_archive_formats() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let dist = rootfs.join("workspace/dist");
        fs::create_dir_all(&dist).unwrap();
        fs::write(dist.join("app.js"), "console.log('ok');").unwrap();

        let tar_path = temp.path().join("artifact.tar");
        let tar_pipeline = smoke_pipeline(temp.path(), ExportFormat::Tar, &tar_path);
        export_pipeline_tar_from_rootfs(&tar_pipeline, &tar_path, &rootfs).unwrap();
        assert_eq!(
            tar_entry_text(File::open(&tar_path).unwrap(), "workspace/dist/app.js"),
            "console.log('ok');"
        );

        let tar_zst_path = temp.path().join("artifact.tar.zst");
        let tar_zst_pipeline = smoke_pipeline(temp.path(), ExportFormat::TarZst, &tar_zst_path);
        export_pipeline_tar_zst_from_rootfs(&tar_zst_pipeline, &tar_zst_path, &rootfs).unwrap();
        let decoder = zstd::Decoder::new(File::open(&tar_zst_path).unwrap()).unwrap();
        assert_eq!(
            tar_entry_text(decoder, "workspace/dist/app.js"),
            "console.log('ok');"
        );
    }

    #[test]
    fn oci_image_export_writes_layout_layer_and_metadata() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let dist = rootfs.join("workspace/dist");
        fs::create_dir_all(&dist).unwrap();
        fs::write(dist.join("app.js"), "console.log('ok');").unwrap();

        let base_image = temp.path().join("base-image");
        let base_layer_digest = write_minimal_base_image(&base_image);
        let output = temp.path().join("image-layout");
        let mut pipeline = smoke_pipeline(temp.path(), ExportFormat::Oci, &output);
        pipeline
            .env
            .insert("APP_ENV".to_string(), "production".to_string());
        pipeline.metadata = Some(ImageMetadata {
            entrypoint: Some(vec!["/bin/sh".to_string(), "-lc".to_string()]),
            cmd: Some(vec!["node /workspace/dist/app.js".to_string()]),
            user: Some("1000:1000".to_string()),
            expose: vec!["8080".to_string()],
            labels: BTreeMap::from([("org.example.app".to_string(), "smoke".to_string())]),
            volumes: vec!["/data".to_string()],
            stop_signal: Some("SIGTERM".to_string()),
            healthcheck: None,
        });

        export_pipeline_oci(&pipeline, &output, &base_image, &rootfs).unwrap();

        assert!(output.join("oci-layout").is_file());
        let manifest = read_layout_manifest(&output);
        let layers = manifest["layers"].as_array().unwrap();
        assert_eq!(layers.len(), 2);
        assert_eq!(
            layers[0]["digest"].as_str().unwrap(),
            format!("sha256:{base_layer_digest}")
        );
        assert_eq!(
            layers[1]["mediaType"].as_str().unwrap(),
            OCI_GZIP_LAYER_MEDIA_TYPE
        );

        let config = read_layout_config(&output, &manifest);
        let env = config
            .pointer("/config/Env")
            .and_then(|value| value.as_array())
            .unwrap();
        assert!(env.iter().any(|value| value == "APP_ENV=production"));
        assert_eq!(config.pointer("/config/WorkingDir").unwrap(), "/workspace");
        assert_eq!(config.pointer("/config/User").unwrap(), "1000:1000");
        assert_eq!(
            config.pointer("/config/Labels/org.example.app").unwrap(),
            "smoke"
        );
        assert!(config.pointer("/config/ExposedPorts/8080~1tcp").is_some());
        assert!(config.pointer("/config/Volumes/~1data").is_some());
        assert_eq!(config.pointer("/config/StopSignal").unwrap(), "SIGTERM");

        let app_layer_digest = layers[1]["digest"].as_str().unwrap();
        let app_layer = File::open(blob_path(&output, app_layer_digest)).unwrap();
        let decoder = flate2::read::GzDecoder::new(app_layer);
        assert_eq!(
            tar_entry_text(decoder, "workspace/dist/app.js"),
            "console.log('ok');"
        );
    }

    fn smoke_pipeline(base_dir: &Path, format: ExportFormat, output_path: &Path) -> Pipeline {
        Pipeline {
            image: "alpine".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: vec![Input {
                source: base_dir.join("src"),
                dest: "/src".to_string(),
                readonly: true,
            }],
            outputs: vec!["/workspace/dist".to_string()],
            setup_snapshot: None,
            operations: vec![Operation::Exec(Step {
                name: Some("build".to_string()),
                run: "echo ok".to_string(),
                run_exec: None,
                run_mounts: Vec::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                build_cache_inputs: None,
                build_cache: None,
                tag: None,
            })],
            export: Some(ExportConfig {
                format,
                path: output_path.to_path_buf(),
                reproducible: true,
            }),
            metadata: None,
            base_dir: base_dir.to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        }
    }

    fn tar_entry_text<R: Read>(reader: R, name: &str) -> String {
        let mut archive = tar::Archive::new(reader);
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            if entry.path().unwrap() == Path::new(name) {
                let mut text = String::new();
                entry.read_to_string(&mut text).unwrap();
                return text;
            }
        }
        panic!("tar archive did not contain {name}");
    }

    fn write_minimal_base_image(base_image: &Path) -> String {
        fs::create_dir_all(base_image).unwrap();

        let base_layer = b"base-layer";
        let base_layer_digest = sha256_hex(base_layer);
        fs::write(base_image.join(&base_layer_digest), base_layer).unwrap();

        let config = serde_json::json!({
            "architecture": "amd64",
            "os": "linux",
            "config": {
                "Env": ["PATH=/usr/local/bin:/usr/bin"],
                "WorkingDir": "/",
            },
            "rootfs": {
                "type": "layers",
                "diff_ids": ["sha256:base-diff"],
            },
            "history": [{
                "created_by": "base image",
            }],
        });
        let config_bytes = serde_json::to_vec_pretty(&config).unwrap();
        let config_digest = sha256_hex(&config_bytes);
        fs::write(base_image.join(&config_digest), &config_bytes).unwrap();

        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": format!("sha256:{config_digest}"),
                "size": config_bytes.len() as u64,
            },
            "layers": [{
                "mediaType": OCI_GZIP_LAYER_MEDIA_TYPE,
                "digest": format!("sha256:{base_layer_digest}"),
                "size": base_layer.len() as u64,
            }],
        });
        fs::write(
            base_image.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        base_layer_digest
    }

    fn read_layout_manifest(layout: &Path) -> serde_json::Value {
        let index: serde_json::Value =
            serde_json::from_slice(&fs::read(layout.join("index.json")).unwrap()).unwrap();
        let manifest_digest = index["manifests"][0]["digest"].as_str().unwrap();
        serde_json::from_slice(&fs::read(blob_path(layout, manifest_digest)).unwrap()).unwrap()
    }

    fn read_layout_config(layout: &Path, manifest: &serde_json::Value) -> serde_json::Value {
        let config_digest = manifest["config"]["digest"].as_str().unwrap();
        serde_json::from_slice(&fs::read(blob_path(layout, config_digest)).unwrap()).unwrap()
    }

    fn blob_path(layout: &Path, digest: &str) -> PathBuf {
        layout
            .join("blobs")
            .join("sha256")
            .join(digest.trim_start_matches("sha256:"))
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex::encode(hasher.finalize())
    }
}
