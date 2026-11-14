//! External texture lookup.
//!
//! Search order: the path as written, the path relative to the model, the
//! model folder plus the file name, then `Textures/` and `textures/` beside
//! the model and in the parent folder. After exact misses, the same folders
//! are scanned case-insensitively. Windows file systems already fold case;
//! the scan is what makes the same assets resolve on Linux, which is where
//! the unit tests run.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ResolveResult {
    pub resolved: Option<PathBuf>,
    pub searched: Vec<String>,
    pub absolute_reference: bool,
}

pub fn resolve(model_path: &Path, referenced: &str) -> ResolveResult {
    let referenced = referenced.trim().trim_matches('"').replace('\\', "/");
    let absolute_reference = is_absolute_ref(&referenced);
    let ref_path = PathBuf::from(&referenced);
    let file_name = ref_path.file_name().map(|n| n.to_os_string());
    let model_dir = model_path.parent().unwrap_or_else(|| Path::new("."));

    let mut candidates: Vec<PathBuf> = Vec::new();
    if ref_path.is_absolute() || absolute_reference {
        candidates.push(PathBuf::from(&referenced));
    }
    if !referenced.is_empty() {
        candidates.push(model_dir.join(&referenced));
    }
    if let Some(name) = &file_name {
        candidates.push(model_dir.join(name));
        candidates.push(model_dir.join("Textures").join(name));
        candidates.push(model_dir.join("textures").join(name));
        if let Some(parent) = model_dir.parent() {
            candidates.push(parent.join("Textures").join(name));
            candidates.push(parent.join("textures").join(name));
            candidates.push(parent.join(name));
        }
    }

    let mut searched = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for candidate in &candidates {
        let key = candidate.to_string_lossy().to_string();
        if !seen.insert(key.clone()) {
            continue;
        }
        searched.push(key);
        if candidate.is_file() {
            return ResolveResult {
                resolved: Some(candidate.clone()),
                searched,
                absolute_reference,
            };
        }
    }

    // Case-insensitive file name match inside the directories we already tried.
    if let Some(name) = &file_name {
        let wanted = name.to_string_lossy().to_string();
        let mut dirs: Vec<PathBuf> = Vec::new();
        dirs.push(model_dir.to_path_buf());
        dirs.push(model_dir.join("Textures"));
        dirs.push(model_dir.join("textures"));
        if let Some(parent) = model_dir.parent() {
            dirs.push(parent.join("Textures"));
            dirs.push(parent.join("textures"));
            dirs.push(parent.to_path_buf());
        }
        if let Some(parent) = ref_path.parent() {
            dirs.push(model_dir.join(parent));
        }
        for dir in dirs {
            let label = format!("{} (case-insensitive)", dir.display());
            if searched.iter().any(|s| s == &label) {
                continue;
            }
            searched.push(label);
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let ename = entry.file_name();
                if ename.to_string_lossy().eq_ignore_ascii_case(&wanted) && entry.path().is_file() {
                    return ResolveResult {
                        resolved: Some(entry.path()),
                        searched,
                        absolute_reference,
                    };
                }
            }
        }
    }

    ResolveResult {
        resolved: None,
        searched,
        absolute_reference,
    }
}

pub fn absolute_path_message(referenced: &str, found: &ResolveResult) -> String {
    let looked = if found.searched.is_empty() {
        "nowhere".to_string()
    } else {
        found.searched.join("; ")
    };
    match &found.resolved {
        Some(path) => format!(
            "texture '{referenced}' is an absolute path from another machine; using '{}' (looked at {looked})",
            path.display()
        ),
        None => format!(
            "texture '{referenced}' is an absolute path from another machine and was not found (looked at {looked})"
        ),
    }
}

fn is_absolute_ref(path: &str) -> bool {
    let b = path.as_bytes();
    if path.starts_with('/') || path.starts_with("//") {
        return true;
    }
    // Windows drive prefix, or a UNC path that lost its backslashes.
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\')
}

pub fn decode_bytes(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    if bytes.len() >= 4 && &bytes[..4] == b"DDS " {
        return decode_dds(bytes);
    }
    let image = image::load_from_memory(bytes).ok()?;
    let rgba = image.to_rgba8();
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 || (w as u64) * (h as u64) > 8192 * 8192 {
        return None;
    }
    Some((w, h, rgba.into_raw()))
}

fn decode_dds(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut cursor = std::io::Cursor::new(bytes);
    let dds = image_dds::ddsfile::Dds::read(&mut cursor).ok()?;
    let image = image_dds::image_from_dds(&dds, 0).ok()?;
    let (w, h) = image.dimensions();
    if w == 0 || h == 0 || (w as u64) * (h as u64) > 8192 * 8192 {
        return None;
    }
    Some((w, h, image.into_raw()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn finds_texture_by_case_insensitive_name() {
        let dir = std::env::temp_dir().join(format!("modelview-tex-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("textures")).unwrap();
        fs::write(dir.join("model.obj"), b"o x\n").unwrap();
        fs::write(dir.join("textures").join("checker.png"), b"png").unwrap();
        let found = resolve(&dir.join("model.obj"), r"Textures/Checker.PNG");
        let resolved = found
            .resolved
            .unwrap_or_else(|| panic!("texture was not found; searched {:?}", found.searched));
        assert_eq!(fs::read(&resolved).unwrap(), b"png");
        assert_eq!(
            resolved
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_ascii_lowercase(),
            "checker.png"
        );
        // macOS and Windows fold case, so Textures/Checker.PNG exists on the
        // first try and the directory scan never runs. A case-sensitive
        // volume misses that path and must record the scan that finds
        // textures/checker.png.
        if filesystem_is_case_sensitive(&dir) {
            assert!(
                found
                    .searched
                    .iter()
                    .any(|entry| entry.contains("case-insensitive")),
                "case-sensitive search should run the directory scan: {:?}",
                found.searched
            );
            assert_eq!(
                resolved.file_name().unwrap().to_string_lossy(),
                "checker.png"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    fn filesystem_is_case_sensitive(dir: &Path) -> bool {
        let upper = dir.join("MVCaseProbe");
        fs::write(&upper, b"probe").unwrap();
        let folded = fs::read(dir.join("mvcaseprobe"));
        let _ = fs::remove_file(&upper);
        folded.is_err()
    }

    #[test]
    fn absolute_windows_path_is_flagged() {
        let found = resolve(Path::new("model.fbx"), r"C:\Art\Textures\a.png");
        assert!(found.absolute_reference);
        assert!(found.resolved.is_none());
        assert!(!found.searched.is_empty());
        let message = absolute_path_message(r"C:\Art\Textures\a.png", &found);
        assert!(message.contains(r"C:\Art\Textures\a.png") || message.contains("C:/Art"));
        assert!(message.contains("not found"));
    }

    #[test]
    fn absolute_path_falls_back_to_the_file_name_near_the_model() {
        let dir = std::env::temp_dir().join(format!("modelview-abs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("Textures")).unwrap();
        fs::write(dir.join("model.fbx"), b"x").unwrap();
        fs::write(dir.join("Textures").join("a.png"), b"png").unwrap();
        let found = resolve(&dir.join("model.fbx"), r"C:\Users\other\Art\Textures\a.png");
        assert!(found.absolute_reference);
        let resolved = found
            .resolved
            .clone()
            .expect("nearby file name should resolve");
        assert_eq!(fs::read(&resolved).unwrap(), b"png");
        let message = absolute_path_message(r"C:\Users\other\Art\Textures\a.png", &found);
        assert!(message.contains("using"));
        assert!(found.searched.iter().any(|s| s.contains("a.png")));
        let _ = fs::remove_dir_all(&dir);
    }
}
