//! マッチ行の整形と色付き出力。

use std::io::{self, Write};
use std::path::Path;

use anstyle::{AnsiColor, Style};

/// 表示用に行を max_columns 文字までに切り詰める（0 なら無制限）。
pub fn truncate_for_display(line: &str, max_columns: usize) -> String {
    if max_columns == 0 {
        return line.to_string();
    }
    let total = line.chars().count();
    if total <= max_columns {
        return line.to_string();
    }
    let head: String = line.chars().take(max_columns).collect();
    format!("{head} …[+{} chars]", total - max_columns)
}

/// マッチ行を `path:line_number:line` 形式で出力する整形器。
///
/// 色 ON のときだけパス・行番号にスタイルを付ける。色 OFF のときは
/// 空スタイルなので、エスケープシーケンスを一切出力しない。
pub struct Printer {
    path: Style,
    line: Style,
}

impl Printer {
    pub fn new(color: bool) -> Self {
        if color {
            Self {
                path: Style::new().fg_color(Some(AnsiColor::Magenta.into())),
                line: Style::new().fg_color(Some(AnsiColor::Green.into())),
            }
        } else {
            Self {
                path: Style::new(),
                line: Style::new(),
            }
        }
    }

    /// マッチ 1 件を出力する。`text` は max_columns で切り詰める。
    pub fn print_match<W: Write>(
        &self,
        out: &mut W,
        path: &Path,
        line_no: usize,
        text: &str,
        max_columns: usize,
    ) -> io::Result<()> {
        let shown = truncate_for_display(text, max_columns);
        write!(
            out,
            "{}{}{}",
            self.path.render(),
            path.display(),
            self.path.render_reset()
        )?;
        write!(
            out,
            ":{}{}{}:",
            self.line.render(),
            line_no,
            self.line.render_reset()
        )?;
        writeln!(out, "{shown}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_lines() {
        assert_eq!(truncate_for_display("hello", 250), "hello");
        assert_eq!(truncate_for_display("hello", 0), "hello");
    }

    #[test]
    fn truncate_long_line_by_chars() {
        // マルチバイト文字でも文字単位で安全に切れること。
        let line = "あ".repeat(300);
        let out = truncate_for_display(&line, 250);
        assert_eq!(out, format!("{} …[+50 chars]", "あ".repeat(250)));
    }

    #[test]
    fn prints_plain_without_color() {
        let printer = Printer::new(false);
        let mut buf = Vec::new();
        printer
            .print_match(&mut buf, Path::new("a/b.txt"), 3, "hello", 0)
            .unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "a/b.txt:3:hello\n");
    }

    #[test]
    fn prints_ansi_with_color() {
        let printer = Printer::new(true);
        let mut buf = Vec::new();
        printer
            .print_match(&mut buf, Path::new("x"), 1, "y", 0)
            .unwrap();
        let s = String::from_utf8(buf).unwrap();
        // ESC を含み、内容も保持されること。
        assert!(s.contains('\u{1b}'));
        assert!(s.contains("x"));
        assert!(s.ends_with(":y\n"));
    }
}
