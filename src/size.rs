//! サイズ文字列（`10M` など）のパース。

use anyhow::{Context, Result};

/// `10M` / `500K` / `2G` / `10MB` / `1024` のようなサイズ文字列をバイト数に変換する。
///
/// 接尾辞は大文字小文字を区別せず、`K`/`KB`/`M`/`MB`/`G`/`GB` を受け付ける。
pub fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    anyhow::ensure!(!s.is_empty(), "empty size");

    // サイズ表記は ASCII 前提なので、接尾辞のバイト長 = 文字数。
    let upper = s.to_ascii_uppercase();
    let (digits, mult): (&str, u64) = if let Some(d) = upper.strip_suffix("KB") {
        (&s[..d.len()], 1024)
    } else if let Some(d) = upper.strip_suffix("MB") {
        (&s[..d.len()], 1024 * 1024)
    } else if let Some(d) = upper.strip_suffix("GB") {
        (&s[..d.len()], 1024 * 1024 * 1024)
    } else if let Some(d) = upper.strip_suffix('K') {
        (&s[..d.len()], 1024)
    } else if let Some(d) = upper.strip_suffix('M') {
        (&s[..d.len()], 1024 * 1024)
    } else if let Some(d) = upper.strip_suffix('G') {
        (&s[..d.len()], 1024 * 1024 * 1024)
    } else {
        (s, 1)
    };

    let n: u64 = digits
        .trim()
        .parse()
        .with_context(|| format!("invalid size value: {s}"))?;
    n.checked_mul(mult)
        .with_context(|| format!("size value is too large: {s}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_units() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("500K").unwrap(), 500 * 1024);
        assert_eq!(parse_size("10M").unwrap(), 10 * 1024 * 1024);
        assert_eq!(parse_size("2g").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("0").unwrap(), 0);
        // KB/MB/GB 表記も受け付ける。
        assert_eq!(parse_size("10MB").unwrap(), 10 * 1024 * 1024);
        assert_eq!(parse_size("2gb").unwrap(), 2 * 1024 * 1024 * 1024);
        assert!(parse_size("abc").is_err());
        assert!(parse_size("").is_err());
        // オーバーフローはエラーにする（パニックさせない）。
        assert!(parse_size("99999999999999G").is_err());
    }
}
