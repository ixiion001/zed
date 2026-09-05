use std::path::{Component, Path, PathBuf};

/// Canonicalization handles symlink aliases. Lexical normalization also supports
/// paths that no longer exist. Windows comparison follows native drive casing.
pub fn normalize(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            _ => result.push(part.as_os_str()),
        }
    }
    #[cfg(windows)]
    let result = {
        let path = result.to_string_lossy().to_lowercase();
        if let Some(unc) = path.strip_prefix(r"\\?\unc\") {
            PathBuf::from(format!(r"\\{unc}"))
        } else {
            PathBuf::from(path.strip_prefix(r"\\?\").unwrap_or(&path))
        }
    };
    Some(result)
}

/// Return the workspace index and its most specific matching root. Multiple
/// roots in one window are fine; equally specific roots in two windows are not.
pub fn select<'a>(
    directory: &Path,
    workspaces: &'a [Vec<PathBuf>],
) -> Result<(usize, &'a Path), &'static str> {
    let directory = normalize(directory).ok_or("invalid-workspace-root")?;
    let mut best = None;
    let mut depth = 0;
    let mut ambiguous = false;
    for (index, roots) in workspaces.iter().enumerate() {
        for root in roots {
            let Some(normalized) = normalize(root) else {
                continue;
            };
            if !directory.starts_with(&normalized) {
                continue;
            }
            let candidate_depth = normalized.components().count();
            if candidate_depth > depth {
                depth = candidate_depth;
                best = Some((index, root.as_path()));
                ambiguous = false;
            } else if candidate_depth == depth && best.is_some_and(|(other, _)| other != index) {
                ambiguous = true;
            }
        }
    }
    if ambiguous {
        Err("ambiguous-workspace")
    } else {
        best.ok_or("no-client-found")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_distinct_duplicate_and_component_boundaries() {
        let base = std::env::temp_dir().join("codex-routing-test");
        let roots = vec![
            vec![base.clone(), base.clone()],
            vec![base.join("nested")],
            vec![base.join("other")],
        ];
        assert_eq!(select(&base.join("nested/src"), &roots).unwrap().0, 1);
        assert_eq!(select(&base.join("other"), &roots).unwrap().0, 2);
        assert_eq!(select(&base.join("nested/../src"), &roots).unwrap().0, 0);
        assert!(select(&base.with_extension("different"), &roots).is_err());
        assert!(select(Path::new("relative"), &roots).is_err());
        let duplicate = vec![vec![base.clone()], vec![base.clone()]];
        assert_eq!(
            select(&base, &duplicate).unwrap_err(),
            "ambiguous-workspace"
        );
        assert!(select(&base, &[]).is_err());
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    #[test]
    fn drive_case_separators_and_verbatim_paths() {
        let roots = vec![vec![PathBuf::from(r"C:\ZedRoutingTest\Repo")]];
        for directory in [
            r"c:/zedroutingtest/repo/src",
            r"\\?\C:\ZedRoutingTest\Repo\src",
        ] {
            assert_eq!(select(Path::new(directory), &roots).unwrap().0, 0);
        }
        assert!(select(Path::new(r"C:\ZedRoutingTest\Repository"), &roots).is_err());
        assert_eq!(
            normalize(Path::new(r"\\?\UNC\server\share\folder")),
            normalize(Path::new(r"\\server\share\folder"))
        );
    }
}
