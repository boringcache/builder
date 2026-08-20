pub(crate) mod context;
mod convert;
mod parse;
pub(crate) mod vars;

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::schema::{ExportConfig, PipelineOrMulti};

pub fn load_dockerfile(
    path: &Path,
    context_dir: &Path,
    platform_override: Option<&str>,
    build_args: &[(String, String)],
    export: Option<ExportConfig>,
) -> Result<PipelineOrMulti> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read Dockerfile {}", path.display()))?;

    let instructions =
        parse::parse(&content).with_context(|| format!("failed to parse {}", path.display()))?;
    let docker_context = context::load(path, context_dir)?;

    convert::convert(
        instructions,
        &docker_context,
        platform_override,
        build_args,
        export,
    )
}

pub fn is_dockerfile(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let lower = name.to_ascii_lowercase();
    lower == "dockerfile" || lower.starts_with("dockerfile.") || lower.ends_with(".dockerfile")
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::load_dockerfile;
    use crate::schema::{Operation, PipelineOrMulti};

    #[test]
    fn load_dockerfile_expands_context_glob_sources() {
        let temp = tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::write(
            temp.path().join("Dockerfile"),
            "FROM alpine\nCOPY src/*.rb /app/\n",
        )
        .unwrap();
        std::fs::write(temp.path().join("src/a.rb"), "puts 'a'\n").unwrap();
        std::fs::write(temp.path().join("src/b.rb"), "puts 'b'\n").unwrap();
        std::fs::write(temp.path().join("src/c.py"), "print('c')\n").unwrap();

        let loaded = load_dockerfile(
            &temp.path().join("Dockerfile"),
            temp.path(),
            None,
            &[],
            None,
        )
        .unwrap();
        let pipeline = match loaded {
            PipelineOrMulti::Single(pipeline) => pipeline,
            PipelineOrMulti::Multi(_) => panic!("expected single-stage pipeline"),
        };

        assert!(matches!(
            &pipeline.operations[0],
            Operation::CopyFromContext(op)
                if op.sources == vec!["src/a.rb".to_string(), "src/b.rb".to_string()]
                    && op.dest == "/app/"
        ));
    }
}
