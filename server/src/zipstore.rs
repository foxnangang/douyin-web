//! 极简 ZIP 打包（仅 Stored 不压缩）。
//!
//! 只为一个场景服务：图集多图打包下载。图片本身已是压缩格式，再 deflate 纯属浪费 CPU，
//! 所以用 Stored 写入即可。手写而不引 zip crate，是为了避开版本 API 差异，
//! 也避免为几十行逻辑多拉一棵依赖树。

use chrono::{Datelike, Timelike};

/// CRC32 查表（IEEE 802.3，多项式 0xEDB88320）
fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *slot = c;
    }
    table
}

fn crc32(data: &[u8]) -> u32 {
    let table = crc_table();
    let mut c = 0xFFFF_FFFFu32;
    for b in data {
        c = table[((c ^ *b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

/// 当前时间的 DOS 格式（date<<16 | time）。ZIP 只支持 1980 年起。
fn dos_datetime() -> (u16, u16) {
    let now = chrono::Local::now();
    let year = (now.year() - 1980).clamp(0, 127) as u16;
    let date = (year << 9) | ((now.month() as u16) << 5) | now.day() as u16;
    let time = ((now.hour() as u16) << 11) | ((now.minute() as u16) << 5) | (now.second() as u16 / 2);
    (date, time)
}

fn push_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn push_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// 打成一个 zip。`files` 是 (文件名, 内容)。
pub fn build_zip(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let (dos_date, dos_time) = dos_datetime();
    let mut out: Vec<u8> = Vec::new();
    // 每个文件的中央目录项先攒着，最后统一追加
    let mut central: Vec<u8> = Vec::new();

    for (name, data) in files {
        let name_bytes = name.as_bytes();
        let crc = crc32(data);
        let size = data.len() as u32;
        let offset = out.len() as u32;

        // ---- 本地文件头 ----
        push_u32(&mut out, 0x0403_4b50); // 签名
        push_u16(&mut out, 20); // 解压所需版本
        push_u16(&mut out, 0x0800); // bit11：文件名是 UTF-8（中文名才不会乱码）
        push_u16(&mut out, 0); // 存储方式：不压缩
        push_u16(&mut out, dos_time);
        push_u16(&mut out, dos_date);
        push_u32(&mut out, crc);
        push_u32(&mut out, size); // 压缩后大小 = 原大小
        push_u32(&mut out, size);
        push_u16(&mut out, name_bytes.len() as u16);
        push_u16(&mut out, 0); // extra 长度
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(data);

        // ---- 中央目录项 ----
        push_u32(&mut central, 0x0201_4b50);
        push_u16(&mut central, 20); // 版本（made by）
        push_u16(&mut central, 20); // 解压所需版本
        push_u16(&mut central, 0x0800);
        push_u16(&mut central, 0);
        push_u16(&mut central, dos_time);
        push_u16(&mut central, dos_date);
        push_u32(&mut central, crc);
        push_u32(&mut central, size);
        push_u32(&mut central, size);
        push_u16(&mut central, name_bytes.len() as u16);
        push_u16(&mut central, 0); // extra
        push_u16(&mut central, 0); // comment
        push_u16(&mut central, 0); // 磁盘号
        push_u16(&mut central, 0); // 内部属性
        push_u32(&mut central, 0); // 外部属性
        push_u32(&mut central, offset); // 本地头偏移
        central.extend_from_slice(name_bytes);
    }

    let cd_offset = out.len() as u32;
    let cd_size = central.len() as u32;
    out.extend_from_slice(&central);

    // ---- 中央目录结束记录 ----
    push_u32(&mut out, 0x0605_4b50);
    push_u16(&mut out, 0); // 本磁盘号
    push_u16(&mut out, 0); // 中央目录起始磁盘
    push_u16(&mut out, files.len() as u16);
    push_u16(&mut out, files.len() as u16);
    push_u32(&mut out, cd_size);
    push_u32(&mut out, cd_offset);
    push_u16(&mut out, 0); // 注释长度

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_known_value() {
        // "123456789" 的标准 CRC32 是 0xCBF43926
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn zip_has_expected_skeleton() {
        let files = vec![
            ("a.txt".to_string(), b"hello".to_vec()),
            ("中文.txt".to_string(), b"world".to_vec()),
        ];
        let z = build_zip(&files);
        // 开头是本地文件头，结尾是 EOCD（22 字节固定结构，无注释）
        assert_eq!(&z[0..4], &[0x50u8, 0x4b, 0x03, 0x04][..]);
        assert_eq!(&z[z.len() - 22..z.len() - 18], &[0x50u8, 0x4b, 0x05, 0x06][..]);
        // 两条目录记录
        let entries = u16::from_le_bytes([z[z.len() - 12], z[z.len() - 11]]);
        assert_eq!(entries, 2);
        // 内容原样嵌入
        assert!(z.windows(5).any(|w| w == b"hello".as_slice()));
        assert!(z.windows(5).any(|w| w == b"world".as_slice()));
    }
}
