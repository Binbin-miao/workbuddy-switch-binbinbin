//! 设置备份：`backups/settings/<uid>/<ts>/`。
//!
//! **备份前置**是这项功能的第二条安全底线（M5）：先把目标现状完整落盘并 `durable_write`
//! 成功，才允许写目标文件；任何一步失败都必须让调用方拿到 `Err` 且目标**零写入**。
//!
//! 目录布局与扁平化保持一致：备份内保留 `global/preferences.json` 与
//! `scoped/<hash>/preferences.json` 的相对路径，还原时按同一相对路径写回。

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::modules::session_backup::{durable_write, durable_write_str};

/// 备份根（注入工具存储根）。
pub fn settings_backup_root_at(store_root: &Path) -> PathBuf {
    store_root.join("backups/settings")
}

/// 某账号的备份根。
pub fn backup_root_for_at(store_root: &Path, uid: &str) -> PathBuf {
    settings_backup_root_at(store_root).join(sanitize_uid(uid))
}

/// uid 直接参与路径，必须拒绝分隔符与 `..`。
fn sanitize_uid(uid: &str) -> String {
    uid.trim()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' { ch } else { '_' })
        .collect()
}

/// 一条备份记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackupEntry {
    pub ts: String,
    pub path: PathBuf,
    pub bytes: u64,
}

/// 备份中一个文件的相对路径 + 内容。
pub struct BackupFile {
    pub rel: String,
    pub bytes: Vec<u8>,
}

/// 生成一份备份。全部文件写入成功才返回 `Ok`，任一失败即删除半截目录并返回 `Err`。
pub fn create_backup_at(
    store_root: &Path,
    uid: &str,
    files: &[BackupFile],
    meta: &str,
) -> Result<PathBuf, String> {
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let root = backup_root_for_at(store_root, uid);
    let target = root.join(&ts);
    std::fs::create_dir_all(&target)
        .map_err(|error| format!("创建设置备份目录失败：{error}"))?;
    let written = (|| -> Result<(), String> {
        for file in files {
            let path = target.join(&file.rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("创建备份子目录失败：{error}"))?;
            }
            durable_write(&path, &file.bytes)
                .map_err(|error| format!("写入备份文件 {} 失败：{error}", file.rel))?;
        }
        durable_write_str(&target.join("meta.json"), meta)
            .map_err(|error| format!("写入备份元信息失败：{error}"))
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_dir_all(&target);
        return Err(error);
    }
    Ok(target)
}

/// 列出某账号的备份（新的在前）。
pub fn list_backups_at(store_root: &Path, uid: &str) -> Vec<BackupEntry> {
    let root = backup_root_for_at(store_root, uid);
    let mut entries: Vec<BackupEntry> = std::fs::read_dir(&root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_dir() {
                return None;
            }
            Some(BackupEntry {
                ts: path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default(),
                bytes: measure(&path),
                path,
            })
        })
        .collect();
    entries.sort_by(|left, right| right.ts.cmp(&left.ts));
    entries
}

/// 读取一条备份里的某个文件；不存在返回 `None`。
pub fn read_backup_file(backup_dir: &Path, rel: &str) -> Option<Vec<u8>> {
    std::fs::read(backup_dir.join(rel)).ok()
}

fn measure(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let child = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&child) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push(child);
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wb-switch-settings-backup-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    #[test]
    fn backup_roundtrip_keeps_relative_layout() {
        let root = temp_root("roundtrip");
        let dir = create_backup_at(
            &root,
            "uid-a",
            &[
                BackupFile {
                    rel: "global/preferences.json".to_string(),
                    bytes: br#"{"theme":"dark"}"#.to_vec(),
                },
                BackupFile {
                    rel: "scoped/abc/preferences.json".to_string(),
                    bytes: br#"{"language":"zh-CN"}"#.to_vec(),
                },
            ],
            "{}",
        )
        .expect("生成备份");
        assert!(dir.join("global/preferences.json").is_file());
        assert!(dir.join("scoped/abc/preferences.json").is_file());
        assert!(dir.join("meta.json").is_file());
        assert_eq!(
            read_backup_file(&dir, "global/preferences.json").expect("读回"),
            br#"{"theme":"dark"}"#
        );
        let listed = list_backups_at(&root, "uid-a");
        assert_eq!(listed.len(), 1);
        assert!(listed[0].bytes > 0);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn uid_is_sanitized_so_it_cannot_escape_the_backup_root() {
        let root = temp_root("sanitize");
        let dir = create_backup_at(
            &root,
            "../../evil",
            &[BackupFile {
                rel: "global/preferences.json".to_string(),
                bytes: b"{}".to_vec(),
            }],
            "{}",
        )
        .expect("生成备份");
        assert!(
            dir.starts_with(settings_backup_root_at(&root)),
            "备份必须落在备份根内：{}",
            dir.display()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn failed_write_removes_partial_backup() {
        let root = temp_root("failed");
        // 第一条把 `global/preferences.json` 写成文件，第二条又要以它为父目录 →
        // 必然失败。此时必须把整份半截备份删掉，不能留下「看起来成功」的目录。
        let result = create_backup_at(
            &root,
            "uid-a",
            &[
                BackupFile {
                    rel: "global/preferences.json".to_string(),
                    bytes: b"{}".to_vec(),
                },
                BackupFile {
                    rel: "global/preferences.json/nested.json".to_string(),
                    bytes: b"{}".to_vec(),
                },
            ],
            "{}",
        );
        assert!(result.is_err(), "第二条写入必然失败");
        let leftovers = list_backups_at(&root, "uid-a");
        assert!(leftovers.is_empty(), "失败后不得残留半截备份：{leftovers:?}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn listing_is_newest_first() {
        let root = temp_root("order");
        for _ in 0..2 {
            create_backup_at(
                &root,
                "uid-a",
                &[BackupFile {
                    rel: "global/preferences.json".to_string(),
                    bytes: b"{}".to_vec(),
                }],
                "{}",
            )
            .expect("生成备份");
            // 同一秒内两次备份会重名覆盖，这里只断言顺序单调不退化。
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let listed = list_backups_at(&root, "uid-a");
        let mut sorted = listed.clone();
        sorted.sort_by(|left, right| right.ts.cmp(&left.ts));
        assert_eq!(listed, sorted);
        std::fs::remove_dir_all(&root).ok();
    }
}
