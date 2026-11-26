use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

fn is_ignored_segment(name: &str) -> bool {
    name == "node_modules" || name == "target" || name == ".git" || name.starts_with('.')
}

/// Normaliza um caminho base de forma léxica (resolvendo `.` e `..` sem exigir que exista no disco).
pub fn absolutize_path(path: &Path) -> io::Result<PathBuf> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut out = PathBuf::new();
    for comp in abs.components() {
        match comp {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(s) => out.push(s),
        }
    }
    Ok(out)
}

/// Lista recursivamente os arquivos visíveis do projeto (ignora ocultos, `.git`, `node_modules` e `target`).
pub fn walk_project_files(dir: &Path, base: &Path) -> io::Result<Vec<String>> {
    let mut out = Vec::new();
    walk_recursive(dir, base, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk_recursive(dir: &Path, base: &Path, out: &mut Vec<String>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if is_ignored_segment(name_str) {
            continue;
        }
        let full = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            walk_recursive(&full, base, out)?;
        } else if file_type.is_file() {
            if let Ok(rel) = full.strip_prefix(base) {
                let rel_str = rel
                    .components()
                    .filter_map(|c| c.as_os_str().to_str())
                    .collect::<Vec<_>>()
                    .join("/");
                if !rel_str.is_empty() {
                    out.push(rel_str);
                }
            }
        }
    }
    Ok(())
}

/// Resolve um caminho relativo dentro de `project_root` bloqueando path traversal, `..`,
/// null bytes, arquivos/diretórios ocultos, `node_modules` e `target`.
pub fn safe_resolve(project_root: &Path, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() || rel.contains('\0') {
        return None;
    }

    // Bloqueia caminhos absolutos ou com prefixo de drive/UNC
    if rel.starts_with('/') || rel.starts_with('\\') || Path::new(rel).is_absolute() {
        return None;
    }
    if rel.as_bytes().get(1) == Some(&b':') {
        return None;
    }

    let mut clean_segments: Vec<&str> = Vec::new();
    for raw_seg in rel.split(['/', '\\']) {
        if raw_seg.is_empty() || raw_seg == "." {
            continue;
        }
        if raw_seg == ".." {
            return None;
        }
        if is_ignored_segment(raw_seg) {
            return None;
        }
        clean_segments.push(raw_seg);
    }

    if clean_segments.is_empty() {
        return None;
    }

    let root = absolutize_path(project_root).ok()?;
    let mut resolved = root.clone();
    for seg in clean_segments {
        resolved.push(seg);
    }

    // Garantia adicional de prefixo após normalização
    if resolved == root || !resolved.starts_with(&root) {
        return None;
    }

    // Se o caminho e a raiz existirem no disco, protege também contra symlinks saindo da raiz
    if let (Ok(canon_root), Ok(canon_resolved)) = (fs::canonicalize(&root), fs::canonicalize(&resolved))
    {
        if !canon_resolved.starts_with(&canon_root) {
            return None;
        }
    }

    Some(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_valid_relative_paths() {
        let root = absolutize_path(Path::new("demo-project")).unwrap();
        assert_eq!(
            safe_resolve(&root, "src/index.ts"),
            Some(root.join("src").join("index.ts"))
        );
        assert_eq!(
            safe_resolve(&root, "package.json"),
            Some(root.join("package.json"))
        );
    }

    #[test]
    fn blocks_traversal_null_hidden_node_modules_and_target() {
        let root = absolutize_path(Path::new("demo-project")).unwrap();
        assert_eq!(safe_resolve(&root, "../demo-project-secret"), None);
        assert_eq!(safe_resolve(&root, "../demo-project-secret/secret.txt"), None);
        assert_eq!(safe_resolve(&root, ".."), None);
        assert_eq!(safe_resolve(&root, "../../etc/passwd"), None);
        assert_eq!(safe_resolve(&root, "src/../secret.txt"), None);
        assert_eq!(safe_resolve(&root, "src\0.ts"), None);
        assert_eq!(safe_resolve(&root, ".env"), None);
        assert_eq!(safe_resolve(&root, ".git"), None);
        assert_eq!(safe_resolve(&root, ".git/config"), None);
        assert_eq!(safe_resolve(&root, "nested/.env.local"), None);
        assert_eq!(safe_resolve(&root, "node_modules/ws/index.js"), None);
        assert_eq!(safe_resolve(&root, "target/debug/app"), None);
        assert_eq!(safe_resolve(&root, "/etc/passwd"), None);
        assert_eq!(safe_resolve(&root, r"C:\Windows\System32"), None);
        assert_eq!(safe_resolve(&root, ""), None);
        assert_eq!(safe_resolve(&root, "."), None);
    }

    #[test]
    fn walk_project_files_ignores_hidden_git_node_modules_and_target() {
        let tmp = std::env::temp_dir().join(format!("swarm_fs_test_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(tmp.join("src")).unwrap();
        fs::create_dir_all(tmp.join(".git")).unwrap();
        fs::create_dir_all(tmp.join("node_modules/pkg")).unwrap();
        fs::create_dir_all(tmp.join("target/debug")).unwrap();

        fs::write(tmp.join("package.json"), "{}").unwrap();
        fs::write(tmp.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(tmp.join(".env"), "SECRET=1").unwrap();
        fs::write(tmp.join(".git/HEAD"), "ref: refs/heads/main").unwrap();
        fs::write(tmp.join("node_modules/pkg/index.js"), "").unwrap();
        fs::write(tmp.join("target/debug/bin"), "").unwrap();

        let files = walk_project_files(&tmp, &tmp).unwrap();
        assert_eq!(files, vec!["package.json", "src/main.rs"]);

        let _ = fs::remove_dir_all(&tmp);
    }
}
