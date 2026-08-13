//! Safe preparation of workspace-local images for platform attachment uploads.

use anyhow::{anyhow, bail, Context, Result};
use image::{ImageFormat, ImageReader};
use std::collections::HashSet;
use std::io::Cursor;
use std::path::{Component, Path};

pub const MAX_WORKSPACE_PNG_ATTACHMENTS: usize = 4;
pub const MAX_WORKSPACE_PNG_BYTES: u64 = 10 * 1024 * 1024;
pub const MAX_WORKSPACE_PNG_TOTAL_BYTES: u64 = 20 * 1024 * 1024;
pub const MAX_WORKSPACE_PNG_PIXELS: u64 = 25_000_000;

#[derive(Debug)]
pub struct PreparedWorkspacePng {
    pub filename: String,
    pub bytes: Vec<u8>,
}

fn safe_relative_png_path(raw: &str) -> Result<&Path> {
    if raw.trim() != raw || raw.is_empty() {
        bail!("attachment path must be a non-empty relative path");
    }
    let path = Path::new(raw);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            ) || matches!(component, Component::Normal(value) if value == ".git")
        })
    {
        bail!("attachment path must stay inside the current workspace: `{raw}`");
    }
    if !path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("png"))
    {
        bail!("only PNG workspace attachments are supported: `{raw}`");
    }
    Ok(path)
}

pub fn prepare_workspace_pngs(
    workspace: &str,
    requested_paths: &[String],
) -> Result<Vec<PreparedWorkspacePng>> {
    if requested_paths.is_empty() {
        return Ok(Vec::new());
    }
    if requested_paths.len() > MAX_WORKSPACE_PNG_ATTACHMENTS {
        bail!("at most {MAX_WORKSPACE_PNG_ATTACHMENTS} workspace PNGs can be attached per reply");
    }

    let root = Path::new(workspace)
        .canonicalize()
        .context("current session workspace is unavailable")?;
    if !root.is_dir() {
        bail!("current session workspace is not a directory");
    }
    if !root.join(".git").exists() {
        bail!("workspace PNG relay is only available for repository-backed sessions");
    }

    let mut prepared = Vec::with_capacity(requested_paths.len());
    let mut filenames = HashSet::new();
    let mut total_bytes = 0u64;
    for raw in requested_paths {
        let relative = safe_relative_png_path(raw)?;
        let resolved = root
            .join(relative)
            .canonicalize()
            .map_err(|_| anyhow!("workspace PNG does not exist: `{raw}`"))?;
        if !resolved.starts_with(&root) {
            bail!("attachment path escapes the current workspace: `{raw}`");
        }
        let metadata = resolved
            .metadata()
            .map_err(|_| anyhow!("cannot inspect workspace PNG: `{raw}`"))?;
        if !metadata.is_file() {
            bail!("workspace attachment is not a regular file: `{raw}`");
        }
        if metadata.len() == 0 {
            bail!("workspace PNG is empty: `{raw}`");
        }
        if metadata.len() > MAX_WORKSPACE_PNG_BYTES {
            bail!("workspace PNG exceeds the 10 MiB limit: `{raw}`");
        }
        total_bytes = total_bytes.saturating_add(metadata.len());
        if total_bytes > MAX_WORKSPACE_PNG_TOTAL_BYTES {
            bail!("workspace PNG attachments exceed the 20 MiB total limit");
        }

        let bytes =
            std::fs::read(&resolved).map_err(|_| anyhow!("cannot read workspace PNG: `{raw}`"))?;
        if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            bail!("workspace attachment is not a valid PNG: `{raw}`");
        }
        let (width, height) = ImageReader::with_format(Cursor::new(&bytes), ImageFormat::Png)
            .into_dimensions()
            .map_err(|_| anyhow!("workspace attachment is not a readable PNG: `{raw}`"))?;
        let pixels = u64::from(width).saturating_mul(u64::from(height));
        if width == 0 || height == 0 || pixels > MAX_WORKSPACE_PNG_PIXELS {
            bail!("workspace PNG dimensions are too large: `{raw}`");
        }

        let filename = resolved
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| anyhow!("workspace PNG filename is not valid UTF-8: `{raw}`"))?
            .to_string();
        if !filenames.insert(filename.to_lowercase()) {
            bail!("workspace PNG filenames must be unique per reply: `{filename}`");
        }
        prepared.push(PreparedWorkspacePng { filename, bytes });
    }
    Ok(prepared)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::RgbImage;
    use std::fs;

    fn write_png(path: &Path, width: u32, height: u32) {
        RgbImage::new(width, height)
            .save_with_format(path, ImageFormat::Png)
            .unwrap();
    }

    fn repository_workspace() -> tempfile::TempDir {
        let workspace = tempfile::tempdir().unwrap();
        fs::create_dir(workspace.path().join(".git")).unwrap();
        workspace
    }

    #[test]
    fn prepares_png_inside_workspace() {
        let workspace = repository_workspace();
        fs::create_dir(workspace.path().join("artifacts")).unwrap();
        write_png(&workspace.path().join("artifacts/preview.png"), 2, 3);

        let prepared = prepare_workspace_pngs(
            workspace.path().to_str().unwrap(),
            &["artifacts/preview.png".into()],
        )
        .unwrap();

        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared[0].filename, "preview.png");
        assert!(prepared[0].bytes.starts_with(b"\x89PNG"));
    }

    #[test]
    fn rejects_absolute_parent_and_non_png_paths() {
        let workspace = repository_workspace();
        for path in ["/tmp/image.png", "../image.png", "notes.txt"] {
            assert!(
                prepare_workspace_pngs(workspace.path().to_str().unwrap(), &[path.into()]).is_err()
            );
        }
    }

    #[test]
    fn rejects_png_extension_with_wrong_magic() {
        let workspace = repository_workspace();
        fs::write(workspace.path().join("fake.png"), b"not an image").unwrap();

        let error =
            prepare_workspace_pngs(workspace.path().to_str().unwrap(), &["fake.png".into()])
                .unwrap_err();

        assert!(error.to_string().contains("not a valid PNG"));
    }

    #[test]
    fn rejects_session_workspace_that_is_not_a_repository() {
        let workspace = tempfile::tempdir().unwrap();
        write_png(&workspace.path().join("preview.png"), 1, 1);

        let error =
            prepare_workspace_pngs(workspace.path().to_str().unwrap(), &["preview.png".into()])
                .unwrap_err();

        assert!(error.to_string().contains("repository-backed"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let workspace = repository_workspace();
        let outside = tempfile::tempdir().unwrap();
        write_png(&outside.path().join("secret.png"), 1, 1);
        symlink(
            outside.path().join("secret.png"),
            workspace.path().join("linked.png"),
        )
        .unwrap();

        let error =
            prepare_workspace_pngs(workspace.path().to_str().unwrap(), &["linked.png".into()])
                .unwrap_err();

        assert!(error.to_string().contains("escapes"));
    }
}
