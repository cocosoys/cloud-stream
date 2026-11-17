//! 最小 zip 读取：枚举中央目录 + 解出单条。
//!
//! 为什么手写而不是引 `zip` crate：这个仓库对新增依赖的取舍是"要有排除其他方案
//! 的理由"（见 `Cargo.toml` 顶部的 Stage1 依赖策略）。这里真正需要的只有两件事 ——
//! 列出条目名（校验主类/`tweaker` 是否在 classpath 里）和解出某一条（解 natives
//! 的 dll），合起来两百行不到；而 `zip` crate 会带进几千行和它自己的 MSRV 约束，
//! 换来的能力（写、seek、zip64、加密）一个都用不上。
//!
//! 只支持 store（0）与 deflate（8）。jar 只会是这两种；遇到别的直接报错，而不是
//! 猜一个 —— natives 解不出来时游戏会以 `UnsatisfiedLinkError` 的形式炸在很远
//! 的地方，远不如启动前说清楚。

use std::io::Read;
use std::path::Path;

const EOCD_SIG: u32 = 0x0605_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const LOCAL_SIG: u32 = 0x0403_4b50;

/// 中央目录里的一条。
///
/// 字段是"解压需要的最小集合"：名字（过滤与落盘）、压缩方法、两处尺寸、
/// 本地头的偏移（真正的数据在那里，不在中央目录里）。
#[derive(Debug, Clone)]
pub struct ZipEntry {
    pub name: String,
    pub method: u16,
    pub comp_size: u64,
    pub uncomp_size: u64,
    pub local_off: u64,
}

/// 读整个 zip 的中央目录。
pub fn entries(path: &Path) -> Result<Vec<ZipEntry>, String> {
    use std::io::{Seek, SeekFrom};

    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let len = f.metadata().map_err(|e| e.to_string())?.len();
    if len < 22 {
        return Err("文件短于 EOCD".to_owned());
    }
    let tail_len = len.min(66_000) as usize; // EOCD + 最长 65535 字节注释
    let tail_start = len - tail_len as u64;
    let mut tail = vec![0u8; tail_len];
    f.seek(SeekFrom::Start(tail_start)).map_err(|e| e.to_string())?;
    f.read_exact(&mut tail).map_err(|e| e.to_string())?;

    let mut eocd: Option<usize> = None;
    for i in (0..=tail_len - 22).rev() {
        let sig = u32::from_le_bytes([tail[i], tail[i + 1], tail[i + 2], tail[i + 3]]);
        if sig != EOCD_SIG {
            continue;
        }
        let comment = u16::from_le_bytes([tail[i + 20], tail[i + 21]]) as usize;
        if i + 22 + comment == tail_len {
            eocd = Some(i);
            break;
        }
    }
    let Some(i) = eocd else { return Err("找不到中央目录结尾（不是 zip？）".to_owned()) };

    let cd_size = u32::from_le_bytes([tail[i + 12], tail[i + 13], tail[i + 14], tail[i + 15]]) as u64;
    let cd_off = u32::from_le_bytes([tail[i + 16], tail[i + 17], tail[i + 18], tail[i + 19]]) as u64;
    if cd_off == u32::MAX as u64 || cd_size == u32::MAX as u64 {
        return Err("zip64 暂不支持".to_owned());
    }
    if cd_off + cd_size > len {
        return Err("中央目录越界".to_owned());
    }

    let cd: Vec<u8> = if cd_off >= tail_start {
        let s = (cd_off - tail_start) as usize;
        let e = (s + cd_size as usize).min(tail_len);
        tail[s..e].to_vec()
    } else {
        let mut buf = vec![0u8; cd_size as usize];
        f.seek(SeekFrom::Start(cd_off)).map_err(|e| e.to_string())?;
        f.read_exact(&mut buf).map_err(|e| e.to_string())?;
        buf
    };
    Ok(central_entries(&cd))
}

/// 解析中央目录段。格式见 PKWARE APPNOTE 4.3.12。
pub fn central_entries(cd: &[u8]) -> Vec<ZipEntry> {
    let mut out = Vec::new();
    let mut p = 0usize;
    while p + 46 <= cd.len() {
        let sig = u32::from_le_bytes([cd[p], cd[p + 1], cd[p + 2], cd[p + 3]]);
        if sig != CENTRAL_SIG {
            break;
        }
        let method = u16::from_le_bytes([cd[p + 10], cd[p + 11]]);
        let comp_size = u32::from_le_bytes([cd[p + 20], cd[p + 21], cd[p + 22], cd[p + 23]]) as u64;
        let uncomp_size = u32::from_le_bytes([cd[p + 24], cd[p + 25], cd[p + 26], cd[p + 27]]) as u64;
        let name_len = u16::from_le_bytes([cd[p + 28], cd[p + 29]]) as usize;
        let extra_len = u16::from_le_bytes([cd[p + 30], cd[p + 31]]) as usize;
        let comment_len = u16::from_le_bytes([cd[p + 32], cd[p + 33]]) as usize;
        let local_off = u32::from_le_bytes([cd[p + 42], cd[p + 43], cd[p + 44], cd[p + 45]]) as u64;
        let start = p + 46;
        let end = start + name_len;
        if end > cd.len() {
            break;
        }
        out.push(ZipEntry {
            name: String::from_utf8_lossy(&cd[start..end]).replace('\\', "/"),
            method,
            comp_size,
            uncomp_size,
            local_off,
        });
        p = end + extra_len + comment_len;
    }
    out
}

/// 只取名（校验主类时用，比 [`entries`] 少一遍结构体分配）。
pub fn names(path: &Path) -> Result<Vec<String>, String> {
    Ok(entries(path)?.into_iter().map(|e| e.name).collect())
}

/// 解出某一条的内容。
pub fn read_entry(path: &Path, entry: &ZipEntry) -> Result<Vec<u8>, String> {
    use std::io::{Seek, SeekFrom};

    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    f.seek(SeekFrom::Start(entry.local_off)).map_err(|e| e.to_string())?;
    let mut head = [0u8; 30];
    f.read_exact(&mut head).map_err(|e| format!("本地头读不出：{e}"))?;
    let sig = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
    if sig != LOCAL_SIG {
        return Err("本地头签名不对".to_owned());
    }
    // 本地头里的名字长度/扩展长度可以与中央目录不同（极少数打包器会这样），
    // 所以数据起点必须从本地头算，不能拿中央目录的值加减。
    let name_len = u16::from_le_bytes([head[26], head[27]]) as usize;
    let extra_len = u16::from_le_bytes([head[28], head[29]]) as usize;
    f.seek(SeekFrom::Start(entry.local_off + 30 + name_len as u64 + extra_len as u64))
        .map_err(|e| e.to_string())?;

    let mut raw = vec![0u8; entry.comp_size as usize];
    f.read_exact(&mut raw).map_err(|e| format!("压缩数据读不全：{e}"))?;

    match entry.method {
        0 => Ok(raw),
        8 => {
            let mut out = Vec::with_capacity(entry.uncomp_size as usize);
            flate2::read::DeflateDecoder::new(&raw[..])
                .read_to_end(&mut out)
                .map_err(|e| format!("deflate 解压失败：{e}"))?;
            Ok(out)
        }
        other => Err(format!("不支持的压缩方法 {other}（只认 store/deflate）")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> Vec<u8> {
        let mut v = vec![0u8; 46];
        v[0..4].copy_from_slice(&CENTRAL_SIG.to_le_bytes());
        v[28..30].copy_from_slice(&(name.len() as u16).to_le_bytes());
        v.extend_from_slice(name.as_bytes());
        v
    }

    #[test]
    fn central_dir_reads_names_in_order() {
        let mut cd = entry("net/minecraft/launchwrapper/Launch.class");
        cd.extend(entry("META-INF/MANIFEST.MF"));
        let got: Vec<String> = central_entries(&cd).into_iter().map(|e| e.name).collect();
        assert_eq!(
            got,
            vec!["net/minecraft/launchwrapper/Launch.class", "META-INF/MANIFEST.MF"]
        );
        assert!(central_entries(&[]).is_empty());
        assert!(central_entries(&[0u8; 46]).is_empty(), "签名不对时不能瞎读");
    }

    #[test]
    fn central_dir_keeps_the_fields_extraction_needs() {
        let mut cd = vec![0u8; 46];
        cd[0..4].copy_from_slice(&CENTRAL_SIG.to_le_bytes());
        cd[10..12].copy_from_slice(&8u16.to_le_bytes()); // deflate
        cd[20..24].copy_from_slice(&1234u32.to_le_bytes());
        cd[24..28].copy_from_slice(&5678u32.to_le_bytes());
        cd[28..30].copy_from_slice(&3u16.to_le_bytes());
        cd[42..46].copy_from_slice(&999u32.to_le_bytes());
        cd.extend_from_slice(b"a/b");
        let e = &central_entries(&cd)[0];
        assert_eq!((e.method, e.comp_size, e.uncomp_size, e.local_off), (8, 1234, 5678, 999));
    }
}
