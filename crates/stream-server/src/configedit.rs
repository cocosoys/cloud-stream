//! 改配置：把"用记事本改 TOML 再祈祷键没拼错"变成一条可校验、可回退的写。
//!
//! `stream-config.toml` 的注释里已经写明三条语义，这里的实现必须和它们对齐：
//!
//! * 键名拼错 = **启动失败并指出是哪个键**（`deny_unknown_fields`），不是静默
//!   忽略。所以写入之前一定要重新解析一遍，让错误停在"保存"这一刻，而不是
//!   停在下次重启。
//! * 空串 `""` 与删掉整行含义不同：前者是"这一项显式留空"，后者是"用默认值"。
//!   因此这里分两个动作：`config-set key=值` 与 `config-unset key`。
//! * 这份文件主要给人看，注释和分组是它的价值。所以改值是**按行替换**，
//!   不是"读成结构体再整个写回去"—— 后者会把用户手写的注释全抹掉，那是
//!   一个只有原作者觉得没问题的操作。
//!
//! # 为什么在 exe 里
//!
//! 和 `ops` 同一个理由：界面上那个表单要知道"这个键现在是什么、这个值合不
//! 合法"，而这两件事都需要真解析器。放进 exe 之后它有单元测试，且 cmd 脚本
//! 也能用；放进界面就得在 GUI 里再写一个 TOML 校验器，两份判断迟早分叉。

use std::fs;
use std::path::Path;

use crate::configfile::FileConfig;

/// 一个键接受的取值范围。刻意只覆盖表单和常见调参用得上的那些：这里多一条
/// 规则，就多一处和真实配置语义分叉的可能。
#[derive(Debug, Clone, Copy)]
pub enum Kind {
    /// 整数，闭区间。
    Num(u64, u64),
    Bool,
    /// 枚举，值必须在这几个里（大小写不敏感，写回时统一成小写）。
    Enum(&'static [&'static str]),
    /// 自由字符串（路径、设备名）。写回时用单引号包住。
    Text,
}

#[derive(Debug, Clone, Copy)]
pub struct Key {
    /// TOML 里的键名，与命令行一一对应（`--some-flag` → `some_flag`）。
    pub name: &'static str,
    pub kind: Kind,
    /// 界面上那一行的大白话，以及这个值是什么单位。命令行列表用它。
    pub label: &'static str,
    /// 表单里那一格的名字。`label` 是给人读的句子（"编码是 CPU 干的，
    /// 虚拟机 15–30 更稳"），塞不进一个 320px 宽的标签；表单要的是名词。
    pub brief: &'static str,
}

pub const CODECS: &[&str] = &["vp8", "vp9"];
pub const ENCODERS: &[&str] = &["auto", "nvenc", "amf", "qsv"];
pub const MODES: &[&str] = &["webrtc", "websockets"];
pub const INPUT_POLICIES: &[&str] = &["exclusive", "shared"];
pub const MIC_DEFAULTS: &[&str] = &["auto", "off"];

/// 可编辑的键。顺序即表单里的顺序。
pub const KEYS: &[Key] = &[
    Key {
        name: "port",
        kind: Kind::Num(1, 65535),
        label: "端口",
        brief: "端口",
    },
    Key {
        name: "width",
        kind: Kind::Num(16, 7680),
        label: "画面宽",
        brief: "画面宽",
    },
    Key {
        name: "height",
        kind: Kind::Num(16, 4320),
        label: "画面高",
        brief: "画面高",
    },
    Key {
        name: "fps",
        kind: Kind::Num(1, 240),
        label: "帧率（编码是 CPU 干的，虚拟机 15–30 更稳）",
        brief: "帧率",
    },
    Key {
        name: "bitrate",
        kind: Kind::Num(50, 200_000),
        label: "码率 kbps（1080p60 用 2500–4000）",
        brief: "码率 kbps",
    },
    Key {
        name: "keyframe_secs",
        kind: Kind::Num(1, 300),
        label: "关键帧间隔（秒）",
        brief: "关键帧间隔（秒）",
    },
    Key {
        name: "repeat_every",
        kind: Kind::Num(1, 600),
        label: "静止画面每 N 拍重发一帧",
        brief: "静止画面重发间隔",
    },
    Key {
        name: "video_codec",
        kind: Kind::Enum(CODECS),
        label: "视频编码（vp9 画质好，vp8 省 CPU）",
        brief: "视频编码",
    },
    Key {
        name: "encoder",
        kind: Kind::Enum(ENCODERS),
        label: "编码器（auto 找不到硬件会落回软件）",
        brief: "编码器",
    },
    Key {
        name: "mode",
        kind: Kind::Enum(MODES),
        label: "传输模式（websockets 只需一个 TCP 端口）",
        brief: "传输模式",
    },
    Key {
        name: "input_policy",
        kind: Kind::Enum(INPUT_POLICIES),
        label: "输入策略（shared = 多人同时操作）",
        brief: "输入策略",
    },
    Key {
        name: "mic_default_device",
        kind: Kind::Enum(MIC_DEFAULTS),
        label: "接管默认录音设备（off = 完全不碰系统设置）",
        brief: "默认录音设备",
    },
    Key {
        name: "display",
        kind: Kind::Num(0, 15),
        label: "采集哪块屏（DXGI 序号，0 = 主屏）",
        brief: "采集哪块屏",
    },
    Key {
        name: "no_hdr",
        kind: Kind::Bool,
        label: "跳过 HDR 采集（虚拟机上设 true）",
        brief: "跳过 HDR 采集",
    },
    // 下面几个是路径/设备名，属于"改一次就不再动"的那类。放进表单是因为它们
    // 恰恰是最容易被手工改坏的一项 —— 路径里的反斜杠在双引号字符串里要写成
    // `\\`，而这个坑谁也记不住。
    Key {
        name: "dist",
        kind: Kind::Text,
        label: "页面目录（不改就留空）",
        brief: "页面目录",
    },
    Key {
        name: "upload_dir",
        kind: Kind::Text,
        label: "上传落盘目录",
        brief: "上传目录",
    },
    Key {
        name: "log",
        kind: Kind::Text,
        label: "日志文件路径",
        brief: "日志文件",
    },
    Key {
        name: "mic_device",
        kind: Kind::Text,
        label: "指定录音设备名（空 = 系统默认）",
        brief: "录音设备名",
    },
];

pub fn find(name: &str) -> Option<&'static Key> {
    KEYS.iter().find(|k| k.name.eq_ignore_ascii_case(name))
}

// ---------------------------------------------------------------------------
// 文本层面：找出一个键所在的行
// ---------------------------------------------------------------------------

/// 一行的解剖结果：`fps = 60  # 注释` → (`fps`, `60`, `# 注释`)。
struct Line<'a> {
    indent: &'a str,
    key: &'a str,
    value: &'a str,
    /// 值后面到 `#` 之间的空白。单独留一份，不然写回时会把
    /// `fps = 30  # 备注` 粘成 `fps = 30# 备注`。
    gap: &'a str,
    /// `#` 起的注释本身，原样保留。
    tail: &'a str,
}

/// 把一行拆成"缩进 + 键 + 值 + 行尾"。
///
/// 只认 `key = value` 这一种形态：TOML 里还有表头、数组、内联表，但这份配置
/// 文件里一个都没有（有的话它们不会被当成键匹配，等于安全地忽略）。
fn dissect(line: &str) -> Option<Line<'_>> {
    // 整行注释与空行不是键。
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let indent_len = line.len() - trimmed.len();
    let indent = &line[..indent_len];
    let eq = trimmed.find('=')?;
    let key = trimmed[..eq].trim();
    if key.is_empty() || key.contains(|c: char| c.is_whitespace()) {
        return None;
    }
    let rest = &trimmed[eq + 1..];
    let (value_part, tail) = split_comment(rest);
    let value = value_part.trim();
    // `trim_end().len()` 而不是 `value.len()`：值前面那段空白也要跳过，
    // 而 `value` 自己的长度从左边数起。
    let gap = &value_part[value_part.trim_end().len()..];
    Some(Line {
        indent,
        key,
        value,
        gap,
        tail,
    })
}

/// 值后面的注释：`'vp9'   # 画质` → 值 `'vp9'  `，行尾 `  # 画质`。
///
/// 要跳过字符串里的 `#`（路径和口令里都可能出现），所以不能简单 `find('#')`。
fn split_comment(rest: &str) -> (&str, &str) {
    let bytes = rest.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if b == b'\\' && q == b'"' {
                    i += 1; // 跳过转义的下一个字符
                } else if b == q {
                    quote = None;
                }
            }
            None => {
                if b == b'\'' || b == b'"' {
                    quote = Some(b);
                } else if b == b'#' {
                    return (&rest[..i], &rest[i..]);
                }
            }
        }
        i += 1;
    }
    (rest, "")
}

/// 找出 `key` 所在行的行号。
fn locate(text: &str, key: &str) -> Option<usize> {
    text.lines()
        .position(|line| dissect(line).map(|l| l.key == key).unwrap_or(false))
}

/// 这份文件用的是哪种换行。
///
/// `lines()` 会把 `\r` 一起吃掉，所以改一行再 `join` 回来，必须补回原来的
/// 写法：Windows 上这份文件本来是 CRLF，若不还原就会变成"每一行都变了"的
/// diff —— 实质上整份文件被重写，`--ops verify` 也会报全文不一致。
fn eol(text: &str) -> &'static str {
    if text.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

/// 文件里这个键写着的原始值（不含行尾注释）。
///
/// 返回的是**文件里写的**，不是生效值：表单要显示"这一行现在是什么"，好让人
/// 在原文上改，而不是看到一堆被默认值填满的格子然后分不清哪些是自己设的。
pub fn raw_value(text: &str, key: &str) -> Option<String> {
    let index = locate(text, key)?;
    let line = text.lines().nth(index)?;
    dissect(line).map(|l| l.value.to_owned())
}

/// 把用户输入按类型格式化成 TOML 里该长成的样子。
pub fn format_value(key: &Key, input: &str) -> Result<String, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err(format!("{} 不能为空（要去掉这一项，请用 unset）", key.name));
    }
    match key.kind {
        Kind::Num(min, max) => {
            let n: u64 = input
                .parse()
                .map_err(|_| format!("{} 要一个整数，收到 {input:?}", key.name))?;
            if n < min || n > max {
                return Err(format!("{} 要在 {min}–{max} 之间，收到 {n}", key.name));
            }
            Ok(n.to_string())
        }
        Kind::Bool => match input.to_ascii_lowercase().as_str() {
            "true" | "1" | "on" | "yes" => Ok("true".to_owned()),
            "false" | "0" | "off" | "no" => Ok("false".to_owned()),
            _ => Err(format!("{} 要 true 或 false，收到 {input:?}", key.name)),
        },
        Kind::Enum(allowed) => {
            let lower = input.to_ascii_lowercase();
            if allowed.contains(&lower.as_str()) {
                Ok(format!("'{lower}'"))
            } else {
                Err(format!(
                    "{} 只能是 {}，收到 {input:?}",
                    key.name,
                    allowed.join(" / ")
                ))
            }
        }
        // 单引号而不是双引号：TOML 的单引号字符串不做转义，Windows 路径里的
        // 反斜杠可以原样写进去，不用管 `\\`。
        Kind::Text => {
            if input.contains('\'') {
                return Err(format!("{} 里不能有单引号（换个写法或直接编辑文件）", key.name));
            }
            Ok(format!("'{input}'"))
        }
    }
}

/// 把 `key` 设成已经格式化好的 `value`。返回新的全文。
pub fn set(text: &str, key: &str, value: &str) -> String {
    if let Some(index) = locate(text, key) {
        let mut out: Vec<String> = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if i != index {
                out.push(line.to_owned());
                continue;
            }
            // 只替换值，保留缩进和行尾注释：注释是这份文件的价值之一。
            let parts = dissect(line).expect("locate 已经确认这一行是键");
            out.push(format!(
                "{}{} = {}{}{}",
                parts.indent, parts.key, value, parts.gap, parts.tail
            ));
        }
        let mut joined = out.join(eol(text));
        if text.ends_with('\n') {
            joined.push_str(eol(text));
        }
        joined
    } else {
        // 键不在文件里：追加到末尾。放进对应的小节需要知道小节结构，而
        // "追加在末尾"永远是合法 TOML —— 好看其次，正确第一。
        let mut out = text.to_owned();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(eol(text));
        }
        out.push_str(&format!("{key} = {value}{}", eol(text)));
        out
    }
}

/// 删掉 `key` 所在的一行（= 回到默认值）。返回新的全文。
pub fn remove(text: &str, key: &str) -> String {
    let Some(index) = locate(text, key) else {
        return text.to_owned();
    };
    let kept: Vec<&str> = text
        .lines()
        .enumerate()
        .filter(|(i, _)| *i != index)
        .map(|(_, line)| line)
        .collect();
    let mut joined = kept.join(eol(text));
    if text.ends_with('\n') {
        joined.push_str(eol(text));
    }
    joined
}

/// 语法与键类型校验。用的是启动时同一套 `FileConfig`，所以"保存能过"等价于
/// "下次启动不会因为这一行失败"。
pub fn validate(text: &str) -> Result<FileConfig, String> {
    toml::from_str::<FileConfig>(text).map_err(|e| describe(e, text))
}

/// 把 toml 的报错翻成"第几行、写了什么"。
///
/// 原始的 `Error` 有 span 但没有行号，而人打开文件时只有行号。这里顺着
/// span 的偏移数一次换行。
fn describe(err: toml::de::Error, text: &str) -> String {
    let span = err.span();
    let line = span
        .map(|s| text[..s.start.min(text.len())].matches('\n').count() + 1)
        .unwrap_or(0);
    let message = err.message().to_owned();
    if line > 0 {
        format!("第 {line} 行：{message}")
    } else {
        message
    }
}

/// 原子写：先写同目录的临时文件，再改名过去。
///
/// 直接 `fs::write` 在断电/崩溃时会留下半截文件，而半截配置的下场是"服务起
/// 不来" —— 恰好是这里最不该制造的状态。改名是原子的，所以要么是旧内容，
/// 要么是新内容。
pub fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, text).map_err(|e| format!("{}: 写临时文件失败: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("{}: 改名失败: {e}", path.display())
    })
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATE: &str = "\
# ===== 头部注释 =====
port = 8080

# ---------- 画面 ----------
width = 1920
height = 1080
# fps: 编码是 CPU 干的。
fps = 60   # 虚拟机 15
video_codec = 'vp9'
no_hdr = true
";

    #[test]
    fn reads_the_raw_value_not_the_effective_one() {
        assert_eq!(raw_value(TEMPLATE, "fps").as_deref(), Some("60"));
        // 没写着的键不能凭空造一个出来：表单里那一格该是空的。
        assert_eq!(raw_value(TEMPLATE, "bitrate"), None);
    }

    /// 行尾注释必须原样留着 —— 那份文件的价值有一半在注释里。
    #[test]
    fn setting_a_value_keeps_the_trailing_comment() {
        let out = set(TEMPLATE, "fps", "30");
        assert!(out.contains("fps = 30   # 虚拟机 15"), "{out}");
        assert_eq!(raw_value(&out, "fps").as_deref(), Some("30"));
    }

    #[test]
    fn setting_a_missing_key_appends_a_line() {
        let out = set(TEMPLATE, "bitrate", "2500");
        assert_eq!(raw_value(&out, "bitrate").as_deref(), Some("2500"));
        // 追加之后仍然是合法 TOML，且原有内容一个字节没动。
        assert!(validate(&out).is_ok());
        assert!(out.starts_with(TEMPLATE));
    }

    /// 删行 = 用默认值，和"写成空串"是两件事。
    #[test]
    fn removing_a_key_is_not_the_same_as_an_empty_value() {
        let removed = remove(TEMPLATE, "no_hdr");
        assert_eq!(raw_value(&removed, "no_hdr"), None);
        assert!(validate(&removed).is_ok());
        // 只删了那一行。
        assert_eq!(removed.lines().count(), TEMPLATE.lines().count() - 1);
    }

    #[test]
    fn comments_and_blank_lines_survive_a_write() {
        let out = set(TEMPLATE, "width", "1280");
        for needle in ["# ===== 头部注释 =====", "# ---------- 画面 ----------", "# fps: 编码是 CPU 干的。"] {
            assert!(out.contains(needle), "注释丢了：{needle}\n{out}");
        }
    }

    #[test]
    fn numbers_are_range_checked() {
        let port = find("port").expect("port 应当在表里");
        assert_eq!(format_value(port, "8080").unwrap(), "8080");
        assert!(format_value(port, "70000").is_err());
        assert!(format_value(port, "80a").is_err());
        assert!(format_value(port, "").is_err());
    }

    #[test]
    fn enums_are_checked_and_normalized() {
        let codec = find("video_codec").expect("video_codec 应当在表里");
        assert_eq!(format_value(codec, "VP9").unwrap(), "'vp9'");
        assert!(format_value(codec, "h265").is_err());
    }

    #[test]
    fn bools_accept_the_usual_spellings() {
        let hdr = find("no_hdr").expect("no_hdr 应当在表里");
        for yes in ["true", "TRUE", "1", "on", "yes"] {
            assert_eq!(format_value(hdr, yes).unwrap(), "true");
        }
        for no in ["false", "0", "off"] {
            assert_eq!(format_value(hdr, no).unwrap(), "false");
        }
        assert!(format_value(hdr, "maybe").is_err());
    }

    /// 一个拼错的键名必须在这里被拦住 —— 让它进文件等于让服务下次拒绝启动。
    #[test]
    fn a_misspelled_key_is_rejected_before_it_reaches_the_file() {
        let broken = set(TEMPLATE, "widht", "1920");
        let err = validate(&broken).expect_err("未知键必须报错");
        assert!(err.contains("第"), "报错要带行号：{err}");
    }

    /// 类型不对也要在保存这一刻失败。
    #[test]
    fn a_wrong_type_is_rejected_with_a_line_number() {
        let broken = set(TEMPLATE, "port", "'8080'");
        let err = validate(&broken).expect_err("类型不对必须报错");
        assert!(err.contains("第 2 行"), "报错要指到那一行：{err}");
    }

    #[test]
    fn string_values_are_single_quoted_so_backslashes_survive() {
        let key = Key {
            name: "log",
            kind: Kind::Text,
            label: "日志路径",
            brief: "日志",
        };
        let value = format_value(&key, r"D:\logs\stream-server.log").unwrap();
        assert_eq!(value, r"'D:\logs\stream-server.log'");
        let out = set(TEMPLATE, "log", &value);
        let parsed = validate(&out).expect("带反斜杠的路径要能过");
        assert_eq!(
            parsed.log_file.as_deref(),
            Some(r"D:\logs\stream-server.log")
        );
    }

    /// `#` 在字符串里时不是注释。
    #[test]
    fn a_hash_inside_a_string_is_not_a_comment() {
        let key = Key {
            name: "app",
            kind: Kind::Text,
            label: "app",
            brief: "app",
        };
        let value = format_value(&key, "we#brtc").unwrap();
        let out = set(TEMPLATE, "app", &value);
        assert_eq!(raw_value(&out, "app").as_deref(), Some("'we#brtc'"));
    }

    /// 改一行不能顺手把整份文件的换行风格改掉：Windows 上这份配置本来是
    /// CRLF，全变成 LF 等于整份文件被重写。
    #[test]
    fn editing_one_line_keeps_the_line_endings() {
        let crlf = "port = 8080\r\nfps = 60  # 虚拟机 15\r\n";
        assert_eq!(
            set(crlf, "fps", "30"),
            "port = 8080\r\nfps = 30  # 虚拟机 15\r\n"
        );
        assert_eq!(remove(crlf, "fps"), "port = 8080\r\n");
    }

    /// `unset` 只是删一行，所以名字认不认识不重要 —— 拼错的键恰恰是最该被删
    /// 掉的那一行，这也是把 `widht = 1920` 救回来唯一不用开记事本的办法。
    #[test]
    fn remove_deletes_a_line_even_when_the_name_is_unknown() {
        assert_eq!(remove(TEMPLATE, "widht"), TEMPLATE, "没有这一行就原样返回");
        let broken = format!("{TEMPLATE}widht = 1920\n");
        assert_eq!(remove(&broken, "widht"), TEMPLATE);
    }

    #[test]
    fn atomic_write_replaces_the_file() {
        let dir = std::env::temp_dir().join(format!("cs-cedit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建目录");
        let path = dir.join("stream-config.toml");
        fs::write(&path, "fps = 60\n").expect("写初值");
        write_atomic(&path, "fps = 30\n").expect("写新值");
        assert_eq!(fs::read_to_string(&path).unwrap(), "fps = 30\n");
        assert!(
            !dir.join("stream-config.toml.tmp").exists(),
            "临时文件不能留下"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
