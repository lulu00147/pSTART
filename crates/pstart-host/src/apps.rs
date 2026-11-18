//! Start Menu application search.

use std::path::{Path, PathBuf};

use pstart_core::{FileId, FileItem, Frn, VolumeId};

fn stable_frn(path: &Path) -> Frn {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in path.to_string_lossy().to_lowercase().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Frn::new(hash)
}

fn collect_shortcuts(root: &Path, depth: usize, output: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten().filter(|entry| {
        entry
            .file_type()
            .is_ok_and(|file_type| !file_type.is_symlink())
    }) {
        let file_type = entry
            .file_type()
            .expect("entries are filtered by readable file type");
        let path = entry.path();
        if file_type.is_dir() {
            collect_shortcuts(&path, depth - 1, output);
        } else if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("lnk"))
        {
            output.push(path);
        }
    }
}

fn score(name: &str, query: &str) -> Option<i32> {
    let name = name.to_lowercase();
    let query = query.to_lowercase();
    if query.is_empty() {
        return None;
    }
    if name == query {
        Some(1000)
    } else if name.starts_with(&query) {
        Some(800 - name.chars().count().min(40) as i32)
    } else if name.contains(&query) {
        Some(400 - name.chars().count().min(40) as i32)
    } else {
        None
    }
}

/// Cached Start Menu shortcut catalog.
#[derive(Clone, Debug, Default)]
pub struct AppCatalog {
    shortcuts: Vec<PathBuf>,
}

impl AppCatalog {
    /// Scan the given roots once.
    #[must_use]
    pub fn load(roots: &[PathBuf]) -> Self {
        let mut shortcuts = Vec::new();
        for root in roots {
            collect_shortcuts(root, 8, &mut shortcuts);
        }
        Self { shortcuts }
    }

    /// Re-scan the given roots.
    pub fn refresh(&mut self, roots: &[PathBuf]) {
        *self = Self::load(roots);
    }

    /// Number of cached shortcuts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.shortcuts.len()
    }

    /// Whether the catalog is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shortcuts.is_empty()
    }

    /// Search cached shortcuts.
    #[must_use]
    pub fn search(&self, query: &str, limit: usize) -> Vec<FileItem> {
        if query.is_empty() || limit == 0 {
            return Vec::new();
        }
        let mut ranked: Vec<(i32, String, PathBuf)> = self
            .shortcuts
            .iter()
            .filter_map(|path| {
                let name = path.file_stem()?.to_string_lossy().into_owned();
                let score = score(&name, query)?;
                Some((score, name, path.clone()))
            })
            .collect();
        ranked.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| left.1.cmp(&right.1))
                .then_with(|| left.2.cmp(&right.2))
        });
        ranked
            .into_iter()
            .take(limit)
            .map(|(_, _, path)| item_for_path(&path))
            .collect()
    }
}

/// Build a stable application item for a shortcut path.
#[must_use]
pub fn item_for_path(path: &Path) -> FileItem {
    let name = path
        .file_stem()
        .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned());
    FileItem {
        id: FileId::new(
            VolumeId::new("apps").expect("fixed volume id"),
            stable_frn(path),
        ),
        name,
        path: path.to_string_lossy().into_owned(),
        attributes: 0,
        is_dir: false,
    }
}

/// Whether a path is an existing `.lnk` shortcut.
#[must_use]
pub fn is_app_shortcut(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("lnk"))
}

/// Search `.lnk` files under the given roots.
#[must_use]
pub fn search_apps(roots: &[PathBuf], query: &str, limit: usize) -> Vec<FileItem> {
    AppCatalog::load(roots).search(query, limit)
}

/// Merge application results into server results, deduplicating by path or ID.
///
/// Returns true when application results were left out because `limit` was
/// already reached.
pub fn merge_results(server: &mut Vec<FileItem>, apps: Vec<FileItem>, limit: usize) -> bool {
    let mut dropped = false;
    for app in apps {
        if server.len() >= limit {
            dropped = true;
            break;
        }
        if server
            .iter()
            .any(|item| item.id == app.id || item.path.eq_ignore_ascii_case(&app.path))
        {
            continue;
        }
        server.push(app);
    }
    dropped
}

/// Default per-user and machine-wide Start Menu roots.
#[must_use]
pub fn start_menu_roots() -> Vec<PathBuf> {
    ["ProgramData", "APPDATA"]
        .into_iter()
        .filter_map(|key| {
            std::env::var_os(key).map(|base| {
                PathBuf::from(base)
                    .join("Microsoft")
                    .join("Windows")
                    .join("Start Menu")
                    .join("Programs")
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::search_apps;

    #[test]
    fn finds_nested_shortcuts_by_name() {
        let dir = tempfile::TempDir::new().unwrap();
        let nested = dir.path().join("Tools");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("Notepad.lnk"), b"x").unwrap();
        std::fs::write(nested.join("Readme.txt"), b"x").unwrap();
        let hits = search_apps(&[dir.path().to_path_buf()], "note", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "Notepad");
        assert!(hits[0].path.ends_with("Notepad.lnk"));
    }

    #[test]
    fn search_is_case_insensitive_and_limited() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("Alpha.lnk"), b"x").unwrap();
        std::fs::write(dir.path().join("Alphabet.lnk"), b"x").unwrap();
        let hits = search_apps(&[dir.path().to_path_buf()], "ALPHA", 1);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "Alpha");
    }

    #[test]
    fn score_covers_match_classes() {
        assert_eq!(super::score("Alpha", "alpha"), Some(1000));
        assert!(super::score("Alphabet", "alpha").unwrap() < 800);
        assert!(super::score("My Alpha", "alpha").unwrap() < 400);
        assert_eq!(super::score("Alpha", "beta"), None);
        assert_eq!(super::score("Alpha", ""), None);
    }

    #[test]
    fn missing_root_and_zero_depth_are_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut found = Vec::new();
        super::collect_shortcuts(dir.path(), 0, &mut found);
        super::collect_shortcuts(&dir.path().join("missing"), 4, &mut found);
        assert!(found.is_empty());
        assert!(search_apps(&[dir.path().to_path_buf()], "", 10).is_empty());
        assert!(search_apps(&[dir.path().to_path_buf()], "a", 0).is_empty());
    }

    #[test]
    fn catalog_refreshes_after_new_shortcut() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut catalog = super::AppCatalog::load(&[dir.path().to_path_buf()]);
        assert!(catalog.is_empty());
        assert_eq!(catalog.len(), 0);
        std::fs::write(dir.path().join("Paint.lnk"), b"x").unwrap();
        assert!(catalog.search("paint", 10).is_empty());
        catalog.refresh(&[dir.path().to_path_buf()]);
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog.search("paint", 10).len(), 1);
    }

    #[test]
    fn app_ids_are_stable_across_catalogs() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("Paint.lnk"), b"x").unwrap();
        let first = super::search_apps(&[dir.path().to_path_buf()], "paint", 10);
        let second = super::search_apps(&[dir.path().to_path_buf()], "paint", 10);
        assert_eq!(first[0].id, second[0].id);
    }

    #[test]
    fn merge_deduplicates_and_caps() {
        let volume = super::VolumeId::new("C:").unwrap();
        let mut server = vec![super::FileItem {
            id: super::FileId::new(volume.clone(), super::Frn::new(1)),
            name: "Paint".to_owned(),
            path: "C:\\Paint.lnk".to_owned(),
            attributes: 0,
            is_dir: false,
        }];
        let apps = vec![
            super::FileItem {
                id: super::FileId::new(super::VolumeId::new("apps").unwrap(), super::Frn::new(2)),
                name: "Paint".to_owned(),
                path: "c:\\paint.lnk".to_owned(),
                attributes: 0,
                is_dir: false,
            },
            super::FileItem {
                id: super::FileId::new(super::VolumeId::new("apps").unwrap(), super::Frn::new(3)),
                name: "Notepad".to_owned(),
                path: "C:\\Notepad.lnk".to_owned(),
                attributes: 0,
                is_dir: false,
            },
            super::FileItem {
                id: super::FileId::new(super::VolumeId::new("apps").unwrap(), super::Frn::new(4)),
                name: "Calc".to_owned(),
                path: "C:\\Calc.lnk".to_owned(),
                attributes: 0,
                is_dir: false,
            },
        ];
        assert!(super::merge_results(&mut server, apps, 2));
        assert_eq!(server.len(), 2);
        assert_eq!(server[1].name, "Notepad");
    }

    #[test]
    fn merge_reports_no_drop_below_limit() {
        let mut server = Vec::new();
        let apps = vec![super::FileItem {
            id: super::FileId::new(super::VolumeId::new("apps").unwrap(), super::Frn::new(2)),
            name: "Calc".to_owned(),
            path: "C:\\Calc.lnk".to_owned(),
            attributes: 0,
            is_dir: false,
        }];
        assert!(!super::merge_results(&mut server, apps, 2));
        assert_eq!(server.len(), 1);
    }

    #[test]
    fn start_menu_roots_follow_environment() {
        std::env::remove_var("ProgramData");
        std::env::remove_var("APPDATA");
        std::env::set_var("ProgramData", "C:\\ProgramData");
        std::env::set_var("APPDATA", "C:\\Users\\test\\AppData\\Roaming");
        let roots = super::start_menu_roots();
        assert_eq!(roots.len(), 2);
        let suffix = PathBuf::from("Start Menu").join("Programs");
        assert!(roots[0].ends_with(&suffix));
        std::env::remove_var("ProgramData");
        let roots = super::start_menu_roots();
        assert_eq!(roots.len(), 1);
        std::env::remove_var("ProgramData");
        std::env::remove_var("APPDATA");
    }
}
