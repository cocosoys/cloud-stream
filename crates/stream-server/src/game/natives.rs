//! natives：把 lwjgl / jinput / openal 的本地库从 jar 里解出来。
//!
//! 旧实现把这一步留给了脚本时代预先铺好的 `natives-tmp` 目录（"存在就用"），
//! 于是换一个客户端目录就得重新手工解一遍，而且**没人记得是哪几个 jar 解出来的**。
//! 现在按版本 json 自己解：`natives` 段说清哪个 jar，`extract.exclude` 说清哪些
//! 条目不要，结果落在 `runtime\natives\<version>\` 下 —— runtime 是每次启动都可以
//! 整个删掉重建的目录，客户端目录保持只读。
//!
//! 三个刻意的保守选择：
//!
//! * **失败不致命**。解不出来时仍然返回旧的 `natives-tmp`（如果它存在）并记一条
//!   提示，而不是拒绝启动 —— 这台机器上现在就跑在那一份上，把它换掉本身是改进，
//!   不该变成新的故障源。
//! * **同尺寸跳过**。dll 不会原地变小又不变大，尺寸相同就当它是对的；这省掉了
//!   每次启动重写十几个 dll（也会撞上"上一个进程还没释放"的共享冲突）。
//! * **只往 runtime 写**。zip 条目名一律校验，带 `..` 或绝对路径的直接拒绝 ——
//!   这是唯一一处"内容来自外面"的落盘路径。

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::zipmin;

/// 一次准备的结果。
#[derive(Debug, Default, Clone)]
pub struct Outcome {
    /// 交给 `-Djava.library.path` 的目录，已按优先级排好。
    pub dirs: Vec<PathBuf>,
    /// 找到的 natives jar 数（找不到的那几个只记在 [`Outcome::notes`] 里）。
    pub jars: usize,
    pub extracted: usize,
    pub reused: usize,
    /// 解压失败的 jar（文件名 + 原因）。
    pub failed: Vec<String>,
    pub notes: Vec<String>,
}

impl Outcome {
    /// `-Djava.library.path` 的值。Windows 的分隔符是 `;`。
    pub fn library_path(&self) -> String {
        self.dirs
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(";")
    }

    /// 一行摘要（`--print-game` 与启动日志共用）。
    pub fn summary(&self) -> String {
        let mut s = format!(
            "natives: {} 个 jar，解出 {} 个文件，{} 个已是最新",
            self.jars, self.extracted, self.reused
        );
        if !self.failed.is_empty() {
            s.push_str(&format!("；{} 个 jar 失败", self.failed.len()));
        }
        s
    }
}

/// 逐库解析并解压。
///
/// `libs` 是**已经过 rules 过滤**的库（调用方手里就有一份，不必再算一遍）。
/// `dest` 是这次版本的落地目录（`runtime\natives\<version>`）。
/// `fallback` 是旧行为那个目录（`natives-tmp` 或版本目录），作为兜底一起进
/// `-Djava.library.path`：万一这个版本的 json 少了某个 natives，老的 dll 还能顶上。
pub fn prepare(
    lib_root: &Path,
    libs: &[&Value],
    dest: &Path,
    fallback: &Path,
) -> Outcome {
    let mut out = Outcome::default();

    for lib in libs {
        let Some(classifier) = windows_classifier(lib) else { continue };
        let Some(jar) = natives_jar(lib_root, lib, &classifier) else {
            out.notes.push(format!(
                "{} 的 natives（{classifier}）在 libraries 里找不到，跳过",
                lib.get("name").and_then(Value::as_str).unwrap_or("?")
            ));
            continue;
        };
        out.jars += 1;
        let exclude = exclude_prefixes(lib);
        match extract(&jar, dest, &exclude, &mut out) {
            Ok(()) => {}
            Err(e) => out.failed.push(format!("{}：{e}", file_name(&jar))),
        }
    }

    if out.jars > 0 && out.extracted + out.reused > 0 {
        out.dirs.push(dest.to_path_buf());
    }
    if fallback.is_dir() && fallback != dest {
        out.dirs.push(fallback.to_path_buf());
    }
    out
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

/// `natives.windows` → classifier 名。`${arch}` 按 64 位替换。
///
/// 老 json（1.12 及以前）写的是固定的 `natives-windows`；新一些的会写
/// `natives-windows-${arch}`，而 vanilla 把 `${arch}` 填成 `32`/`64` —— 这里填 64，
/// 因为整个产品只跑 x86_64（`rules` 的判据也是这么写的）。
fn windows_classifier(lib: &Value) -> Option<String> {
    let raw = lib.pointer("/natives/windows").and_then(Value::as_str)?;
    let name = raw.replace("${arch}", "64");
    if name.trim().is_empty() {
        None
    } else {
        Some(name)
    }
}

/// natives jar 的绝对路径：优先 `downloads.classifiers`，否则按 maven 布局推。
fn natives_jar(lib_root: &Path, lib: &Value, classifier: &str) -> Option<PathBuf> {
    let by_path = lib
        .pointer("/downloads/classifiers")
        .and_then(|c| c.get(classifier))
        .and_then(|c| c.get("path"))
        .and_then(Value::as_str);
    let rel = match by_path {
        Some(p) => p.replace('/', "\\"),
        None => {
            let name = lib.get("name").and_then(Value::as_str)?;
            maven_path(name, classifier)?
        }
    };
    let full = lib_root.join(rel);
    if full.is_file() {
        Some(full)
    } else {
        None
    }
}

/// `group:artifact:version` + classifier →
/// `group\artifact\version\artifact-version-classifier.jar`
fn maven_path(name: &str, classifier: &str) -> Option<String> {
    let mut parts = name.split(':');
    let g = parts.next()?;
    let a = parts.next()?;
    let v = parts.next()?;
    // 四段（`g:a:v:classifier`）已经带了 classifier，不重复拼。
    let existing = parts.next();
    let cls = existing.unwrap_or(classifier);
    Some(format!(
        "{}\\{}\\{}\\{}-{}-{}.jar",
        g.replace('.', "\\"),
        a,
        v,
        a,
        v,
        cls
    ))
}

/// `extract.exclude` 的前缀列表。缺省是 `META-INF/`（vanilla 的默认）。
fn exclude_prefixes(lib: &Value) -> Vec<String> {
    let list = lib
        .pointer("/extract/exclude")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(|s| s.replace('\\', "/"))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if list.is_empty() {
        vec!["META-INF/".to_owned()]
    } else {
        list
    }
}

/// 把一个 jar 里的条目解到 `dest`。
fn extract(jar: &Path, dest: &Path, exclude: &[String], out: &mut Outcome) -> Result<(), String> {
    let entries = zipmin::entries(jar)?;
    let mut wrote = 0usize;
    let mut skipped = 0usize;
    for entry in entries {
        let name = entry.name.as_str();
        if name.ends_with('/') || name.is_empty() {
            continue;
        }
        if exclude.iter().any(|p| name.starts_with(p.as_str())) {
            continue;
        }
        // zip slip：条目名来自外部，绝对不能让它把文件写到 dest 之外。
        if name.starts_with('/') || name.contains(':') || name.split('/').any(|s| s == "..") {
            out.notes.push(format!("{}：条目 {name} 的路径可疑，跳过", file_name(jar)));
            continue;
        }
        let target = dest.join(name.replace('/', "\\"));
        if let Ok(meta) = std::fs::metadata(&target) {
            if meta.len() == entry.uncomp_size {
                skipped += 1;
                out.reused += 1;
                continue;
            }
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("建目录 {} 失败：{e}", parent.display()))?;
        }
        let data = zipmin::read_entry(jar, &entry)?;
        if data.len() as u64 != entry.uncomp_size {
            return Err(format!(
                "{name} 解出来 {} 字节，中央目录说 {} —— 文件可能被截断",
                data.len(),
                entry.uncomp_size
            ));
        }
        std::fs::write(&target, &data)
            .map_err(|e| format!("写 {} 失败：{e}", target.display()))?;
        out.extracted += 1;
        wrote += 1;
    }
    if wrote == 0 && skipped == 0 {
        return Err("没有可解出的条目（extract.exclude 把它全滤掉了？）".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifier_substitutes_arch() {
        let old = serde_json::json!({"natives": {"windows": "natives-windows"}});
        assert_eq!(windows_classifier(&old).as_deref(), Some("natives-windows"));
        let new = serde_json::json!({"natives": {"windows": "natives-windows-${arch}"}});
        assert_eq!(windows_classifier(&new).as_deref(), Some("natives-windows-64"));
        let mac = serde_json::json!({"natives": {"osx": "natives-osx"}});
        assert_eq!(windows_classifier(&mac), None, "没有 windows 段的库不参与");
    }

    #[test]
    fn maven_path_appends_the_classifier() {
        assert_eq!(
            maven_path("org.lwjgl.lwjgl:lwjgl-platform:2.9.4-nightly-20150209", "natives-windows")
                .as_deref(),
            Some(
                r"org\lwjgl\lwjgl\lwjgl-platform\2.9.4-nightly-20150209\lwjgl-platform-2.9.4-nightly-20150209-natives-windows.jar"
            )
        );
        // 名字里已经带 classifier 时不重复拼。
        assert_eq!(
            maven_path("a.b:c:1:natives-windows", "natives-windows").as_deref(),
            Some(r"a\b\c\1\c-1-natives-windows.jar")
        );
    }

    #[test]
    fn exclude_defaults_to_meta_inf() {
        let lib = serde_json::json!({"name": "a:b:1"});
        assert_eq!(exclude_prefixes(&lib), vec!["META-INF/".to_owned()]);
        let custom = serde_json::json!({"extract": {"exclude": ["META-INF/", "docs/"]}});
        assert_eq!(exclude_prefixes(&custom).len(), 2);
    }
}
