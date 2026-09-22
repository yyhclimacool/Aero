//! 默认示例库（`ui_design.md` 11：库选择页上的一个入口，不在启动时自动打开）。
//!
//! 六篇笔记的源码在 `assets/demo-vault/`，编译期嵌入二进制，运行时不读安装包里的文件，
//! 所以每个 `.dmg` 带的就是那次构建的示例。用户每次从库选择页进来都会走 [`prepare`]：
//! - 目录不存在：先在同级临时目录写完整副本再整体 `rename` 发布，中途失败只留错误，不把半个库交给用户；
//! - 目录已存在：按 `.aero/demo.toml` 里记录的"上次交付指纹"增量更新。文件还是我们上次写进去的样子
//!   就换成这一版的内容，用户改过的不动，删掉或改名过的不恢复，这一版新增的示例才补写。
//!
//! 没有 `.aero/demo.toml` 的同名目录一个字都不写：那是用户自己的文件夹，不是我们交付的示例库。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use aero_core::{Vault, VaultError, VaultPath};
use serde::{Deserialize, Serialize};

pub const WELCOME: &str = "00-欢迎.md";
const DIR: &str = "demo-vault";
const NOTES: &[(&str, &str)] = &[
    (WELCOME, include_str!("../../../assets/demo-vault/00-欢迎.md")),
    ("体验/Markdown 排版.md", include_str!("../../../assets/demo-vault/体验/Markdown 排版.md")),
    ("体验/双链与别名.md", include_str!("../../../assets/demo-vault/体验/双链与别名.md")),
    ("体验/标题与块引用.md", include_str!("../../../assets/demo-vault/体验/标题与块引用.md")),
    ("项目/项目清单.md", include_str!("../../../assets/demo-vault/项目/项目清单.md")),
    ("练习/移动与改名.md", include_str!("../../../assets/demo-vault/练习/移动与改名.md")),
];

/// 示例库在应用数据目录里的位置。
pub fn root(data_dir: &Path) -> PathBuf {
    data_dir.join(DIR)
}

/// 初始化或更新示例库，返回可以打开的目录。
pub fn prepare(data_dir: &Path) -> Result<PathBuf, String> {
    let root = root(data_dir);
    if existing_directory(&root)? {
        update(&root)?;
    } else {
        initialize(data_dir, &root)?;
    }
    Ok(root)
}

fn initialize(data_dir: &Path, root: &Path) -> Result<(), String> {
    fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let staging = data_dir.join(format!(".{DIR}-{}-{nonce}", std::process::id()));
    fs::create_dir(&staging).map_err(|e| e.to_string())?;
    // 只发布完整的库：准备失败不会变成用户的笔记库。
    let result = (|| {
        let vault = Vault::open(&staging).map_err(|e| e.to_string())?;
        let mut manifest = Manifest::default();
        for (path, source) in NOTES {
            let stamp = vault
                .create_bytes(&VaultPath::new(path).expect("bundled note path"), source.as_bytes())
                .map_err(|e| e.to_string())?;
            manifest.record(path, stamp.hash);
        }
        manifest.save(&staging)?;
        if !existing_directory(root)? {
            if let Err(error) = fs::rename(&staging, root) {
                if !existing_directory(root)? {
                    return Err(error.to_string());
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = fs::remove_dir_all(&staging) {
        if error.kind() != std::io::ErrorKind::NotFound {
            log::warn!("demo staging cleanup {}: {error}", staging.display());
        }
    }
    result
}

/// 把这一版的示例内容交付到已有目录：只改还是上次交付原样的文件，其余一概不碰。
fn update(root: &Path) -> Result<(), String> {
    // 没有交付记录就不是我们的示例库（用户自己建的同名文件夹），不写任何东西。
    let Some(mut manifest) = Manifest::load(root) else { return Ok(()) };
    let vault = Vault::open(root).map_err(|e| e.to_string())?;
    let mut delivered = 0;
    for (path, source) in NOTES {
        let note = VaultPath::new(path).expect("bundled note path");
        let write = match vault.read_bytes(&note) {
            // 已经是这一版的内容：顺手把基线记上，老库也能跟上后续更新。
            Ok((bytes, stamp)) if bytes == source.as_bytes() => {
                manifest.record(path, stamp.hash);
                false
            }
            // 内容还是上次交付的样子，用户没动过，可以换成新版。
            Ok((_, stamp)) => manifest.delivered(path) == Some(stamp.hash),
            // 交付过的笔记被删掉或改名了就不再恢复；这一版新增的示例才补写。
            Err(error) if missing(&error) => !manifest.knows(path),
            Err(error) => return Err(error.to_string()),
        };
        if write {
            let stamp = vault.write_bytes(&note, source.as_bytes()).map_err(|e| e.to_string())?;
            manifest.record(path, stamp.hash);
            delivered += 1;
        }
    }
    if delivered > 0 {
        log::info!("demo vault: {delivered} notes updated to the bundled version");
    }
    manifest.save(root)
}

/// `.aero/demo.toml`：每篇示例最近一次交付时的内容指纹（`FileStamp` 的 xxh3-64，十六进制）。
/// 放在 `.aero/` 里，不会出现在文件树和索引中。
#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    #[serde(default)]
    delivered: BTreeMap<String, String>,
}

impl Manifest {
    fn file(root: &Path) -> PathBuf {
        root.join(".aero").join("demo.toml")
    }

    /// 读不到或读不懂都返回 `None`：宁可不更新，也不冒覆盖用户笔记的风险。
    fn load(root: &Path) -> Option<Manifest> {
        toml::from_str(&fs::read_to_string(Self::file(root)).ok()?).ok()
    }

    fn save(&self, root: &Path) -> Result<(), String> {
        let path = Manifest::file(root);
        fs::create_dir_all(path.parent().expect("manifest parent")).map_err(|e| e.to_string())?;
        let body = toml::to_string(self).map_err(|e| e.to_string())?;
        fs::write(&path, body).map_err(|e| format!("{}: {e}", path.display()))
    }

    fn knows(&self, path: &str) -> bool {
        self.delivered.contains_key(path)
    }

    fn delivered(&self, path: &str) -> Option<u64> {
        self.delivered.get(path).and_then(|hash| u64::from_str_radix(hash, 16).ok())
    }

    fn record(&mut self, path: &str, hash: u64) {
        self.delivered.insert(path.to_owned(), format!("{hash:016x}"));
    }
}

fn missing(error: &VaultError) -> bool {
    matches!(error, VaultError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound)
}

fn existing_directory(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(true),
        Ok(_) => Err(format!("{} 不是普通目录，无法打开示例库", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::{Manifest, NOTES, WELCOME, prepare};
    use aero_core::{
        Vault, VaultPath,
        links::{Resolution, Resolver},
    };
    use std::fs;

    fn bundled(path: &str) -> &'static str {
        NOTES.iter().find(|(p, _)| *p == path).expect("bundled note").1
    }

    /// 内嵌表就是随 `.dmg` 分发的示例：漏登记一篇新示例必须在这里失败，而不是等用户装完发现少一篇。
    #[test]
    fn bundled_notes_match_the_repository_assets() {
        let assets = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/demo-vault");
        let mut on_disk = Vec::new();
        let mut stack = vec![assets.clone()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|x| x == "md") {
                    on_disk.push(path.strip_prefix(&assets).unwrap().to_string_lossy().into_owned());
                }
            }
        }
        on_disk.sort();
        let mut embedded: Vec<String> = NOTES.iter().map(|(path, _)| (*path).to_owned()).collect();
        embedded.sort();
        assert_eq!(embedded, on_disk, "assets/demo-vault 与内嵌表不一致");
        for (path, source) in NOTES {
            assert_eq!(
                *source,
                fs::read_to_string(assets.join(path)).unwrap(),
                "{path} 的内嵌副本不是最新的"
            );
        }
    }

    #[test]
    fn demo_vault_initializes_six_connected_notes() {
        let dir = tempfile::tempdir().unwrap();
        let root = prepare(dir.path()).unwrap();
        assert_eq!(root, dir.path().join("demo-vault"));
        let vault = Vault::open(&root).unwrap();
        let notes = vault.notes().unwrap();
        assert_eq!(notes.len(), 6);
        assert!(notes.contains(&VaultPath::new(WELCOME).unwrap()));
        let mut resolver = Resolver::new();
        for path in &notes {
            let (doc, _) = vault.read(path).unwrap();
            resolver.insert(
                path.clone(),
                doc.front_matter_title().as_ref().map(|(_, t)| t.as_str()),
                &doc.front_matter_aliases(),
            );
        }
        let mut missing = Vec::new();
        for path in notes {
            let (doc, _) = vault.read(&path).unwrap();
            for link in aero_core::links::scan(&doc.to_string()) {
                let target = if link.target.is_empty() {
                    path.clone()
                } else {
                    match resolver.resolve(&link.target, link.fragment.as_deref(), path.parent()) {
                        Resolution::Resolved { path, .. } => path,
                        Resolution::Unresolved => {
                            missing.push(link.target);
                            continue;
                        }
                        Resolution::Ambiguous { .. } => panic!("ambiguous demo link: {}", link.target),
                    }
                };
                if let Some(fragment) = link.fragment.as_deref() {
                    let (target, _) = vault.read(&target).unwrap();
                    if let Some(id) = fragment.strip_prefix('^') {
                        assert!(aero_core::block_refs::resolve(&target, id).is_ok());
                    } else {
                        assert!(
                            aero_core::headings::list(&target).iter().any(|h| h.text == fragment),
                            "missing demo heading: {fragment}"
                        );
                    }
                }
            }
        }
        assert_eq!(missing, ["下一篇灵感笔记"]);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1, "no staging leftovers");
        assert_eq!(Manifest::load(&root).unwrap().delivered.len(), 6, "交付记录覆盖全部示例");
    }

    #[test]
    fn demo_vault_reopen_preserves_edits_deletions_and_renames() {
        let dir = tempfile::tempdir().unwrap();
        let first = prepare(dir.path()).unwrap();
        let vault_id = Vault::open(&first).unwrap().id().to_owned();
        fs::write(first.join(WELCOME), "用户自己的欢迎页\n").unwrap();
        fs::remove_file(first.join("体验/Markdown 排版.md")).unwrap();
        fs::rename(first.join("练习/移动与改名.md"), first.join("练习/改过名字.md")).unwrap();
        fs::write(dir.path().join("state.toml"), "recent_vaults = ['/another/vault']\n").unwrap();
        let next = prepare(dir.path()).unwrap();
        assert_eq!(next, first);
        assert_eq!(fs::read_to_string(next.join(WELCOME)).unwrap(), "用户自己的欢迎页\n");
        assert!(!next.join("体验/Markdown 排版.md").exists());
        assert!(!next.join("练习/移动与改名.md").exists());
        assert!(next.join("练习/改过名字.md").is_file());
        assert_eq!(Vault::open(&next).unwrap().id(), vault_id);
    }

    /// 新版发布后再次选择示例库：没动过的示例换成新内容，动过的、删掉的一概不碰，新增的补上。
    #[test]
    fn demo_vault_update_delivers_new_content_without_touching_user_notes() {
        let dir = tempfile::tempdir().unwrap();
        let root = prepare(dir.path()).unwrap();
        let vault = Vault::open(&root).unwrap();
        let (untouched, edited, added) = ("体验/双链与别名.md", "项目/项目清单.md", "体验/标题与块引用.md");
        // 把库改成"上一版"的样子：交付记录跟着内容走的那篇没被用户动过，另一篇是用户自己改的。
        let mut manifest = Manifest::load(&root).unwrap();
        let old = "---\ntitle: 上一版\n---\n\n旧的示例内容\n";
        let stamp = vault.write_bytes(&VaultPath::new(untouched).unwrap(), old.as_bytes()).unwrap();
        manifest.record(untouched, stamp.hash);
        fs::write(vault.fs_path(&VaultPath::new(edited).unwrap()), "我自己的清单\n").unwrap();
        // 这一版新增的示例：交付记录里没有，文件也不在。
        manifest.delivered.remove(added);
        fs::remove_file(vault.fs_path(&VaultPath::new(added).unwrap())).unwrap();
        manifest.save(&root).unwrap();

        assert_eq!(prepare(dir.path()).unwrap(), root);
        assert_eq!(fs::read_to_string(root.join(untouched)).unwrap(), bundled(untouched));
        assert_eq!(fs::read_to_string(root.join(edited)).unwrap(), "我自己的清单\n");
        assert_eq!(fs::read_to_string(root.join(added)).unwrap(), bundled(added));
        // 用户改过的那篇基线仍是首次交付的指纹，以后发版也不会被悄悄覆盖。
        let manifest = Manifest::load(&root).unwrap();
        assert_ne!(manifest.delivered(edited), None);
        assert_eq!(prepare(dir.path()).unwrap(), root);
        assert_eq!(fs::read_to_string(root.join(edited)).unwrap(), "我自己的清单\n");
    }

    #[test]
    fn demo_vault_never_populates_a_directory_it_did_not_deliver() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("demo-vault");
        fs::create_dir(&existing).unwrap();
        fs::write(existing.join("mine.md"), "keep\n").unwrap();
        assert_eq!(prepare(dir.path()).unwrap(), existing);
        assert_eq!(fs::read_dir(&existing).unwrap().count(), 1);
        assert_eq!(fs::read_to_string(existing.join("mine.md")).unwrap(), "keep\n");
    }

    #[test]
    fn demo_vault_does_not_restore_a_deleted_welcome_page() {
        let dir = tempfile::tempdir().unwrap();
        let root = prepare(dir.path()).unwrap();
        fs::remove_file(root.join(WELCOME)).unwrap();
        assert_eq!(prepare(dir.path()).unwrap(), root);
        assert!(!root.join(WELCOME).exists());
    }

    #[gpui_kit::test]
    fn demo_vault_notes_render_in_the_editor(cx: &mut gpui_kit::TestAppContext) {
        use std::sync::Arc;
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::actions::init(cx);
        });
        let dir = tempfile::tempdir().unwrap();
        let root = prepare(dir.path()).unwrap();
        let vault = Arc::new(Vault::open(&root).unwrap());
        for path in vault.notes().unwrap() {
            let (doc, stamp) = vault.read(&path).unwrap();
            let title = doc.title_or(path.stem());
            let (editor, visual) =
                cx.add_window_view(|_, cx| crate::editor::Editor::new(vault.clone(), path, doc, stamp, cx));
            visual.run_until_parked();
            visual.update(|window, cx| {
                window.draw(cx).clear(cx);
                assert_eq!(editor.read(cx).title(), title);
                assert_eq!(editor.read(cx).index_snapshot().2.hash, stamp.hash);
            });
            assert!(visual.debug_bounds("editor-title").is_some());
        }
    }

    #[test]
    fn demo_vault_reports_file_conflict_without_overwriting_it() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("demo-vault"), "not a directory").unwrap();
        assert!(prepare(dir.path()).is_err());
        assert_eq!(fs::read_to_string(dir.path().join("demo-vault")).unwrap(), "not a directory");
    }

    #[cfg(unix)]
    #[test]
    fn demo_vault_rejects_a_symlink_without_writing_through_it() {
        let dir = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(target.path(), dir.path().join("demo-vault")).unwrap();
        assert!(prepare(dir.path()).is_err());
        assert_eq!(fs::read_dir(target.path()).unwrap().count(), 0);
    }
}
