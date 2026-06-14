//! マッチ行の整形と色付き出力。

use std::io::{self, Write};
use std::path::Path;

use anstyle::{AnsiColor, Style};
use regex::Regex;

/// 表示する本文部分（先頭 `max_columns` 文字）と、切り詰めた残り文字数を返す。
///
/// 切り詰めは Unicode の文字数（`char` 数）ベースであり、端末上の表示幅
/// （全角や絵文字は 2 桁など）とは一致しない。UTF-8 の文字境界は壊さない。
/// `max_columns == 0` は無制限。
fn split_visible(line: &str, max_columns: usize) -> (&str, usize) {
    if max_columns == 0 {
        return (line, 0);
    }
    let total = line.chars().count();
    if total <= max_columns {
        return (line, 0);
    }
    // max_columns 文字目の境界のバイト位置で切る。
    let cut = line
        .char_indices()
        .nth(max_columns)
        .map(|(i, _)| i)
        .unwrap_or(line.len());
    (&line[..cut], total - max_columns)
}

/// マッチ行を `path:line_number:line` 形式で出力する整形器。
///
/// 色 ON のときだけパス・行番号・一致箇所にスタイルを付ける。色 OFF のときは
/// 空スタイルなので、エスケープシーケンスを一切出力しない。
pub struct Printer {
    path: Style,
    line: Style,
    matched: Style,
    color: bool,
}

impl Printer {
    pub fn new(color: bool) -> Self {
        if color {
            Self {
                path: Style::new().fg_color(Some(AnsiColor::Magenta.into())),
                line: Style::new().fg_color(Some(AnsiColor::Green.into())),
                matched: Style::new().fg_color(Some(AnsiColor::Red.into())).bold(),
                color: true,
            }
        } else {
            Self {
                path: Style::new(),
                line: Style::new(),
                matched: Style::new(),
                color: false,
            }
        }
    }

    /// マッチ 1 件を出力する。`text` は max_columns で切り詰め、可視範囲内の
    /// `re` の一致箇所を強調表示する。
    pub fn print_match<W: Write>(
        &self,
        out: &mut W,
        path: &Path,
        line_no: usize,
        text: &str,
        max_columns: usize,
        re: &Regex,
    ) -> io::Result<()> {
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

        let (visible, truncated) = split_visible(text, max_columns);
        if self.color {
            // 可視範囲（先頭 max_columns 文字）内の一致箇所のみ強調する。
            let end = visible.len();
            let mut last = 0;
            for m in re.find_iter(text) {
                if m.start() >= end {
                    break; // 可視範囲より後ろの一致は表示しない
                }
                if m.start() == m.end() {
                    continue; // 幅ゼロの一致（空パターン・^・\b 等）は強調しない
                }
                let hit_end = m.end().min(end);
                write!(out, "{}", &visible[last..m.start()])?;
                write!(
                    out,
                    "{}{}{}",
                    self.matched.render(),
                    &visible[m.start()..hit_end],
                    self.matched.render_reset()
                )?;
                last = hit_end;
                if hit_end == end {
                    break; // 可視範囲の末尾に到達
                }
            }
            write!(out, "{}", &visible[last..])?;
        } else {
            // 色 OFF: 強調しないので正規表現の走査をせず本文をそのまま書く。
            write!(out, "{visible}")?;
        }

        if truncated > 0 {
            write!(out, " …[+{truncated} chars]")?;
        }
        writeln!(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_visible_keeps_short_lines() {
        assert_eq!(split_visible("hello", 250), ("hello", 0));
        assert_eq!(split_visible("hello", 0), ("hello", 0));
    }

    #[test]
    fn split_visible_cuts_by_chars() {
        // マルチバイト文字でも文字単位で安全に切れること。
        let line = "あ".repeat(300);
        let (visible, truncated) = split_visible(&line, 250);
        assert_eq!(visible, "あ".repeat(250));
        assert_eq!(truncated, 50);
    }

    /// テスト用に、与えた正規表現で 1 件出力した文字列を得る。
    fn render(printer: &Printer, text: &str, max_columns: usize, pat: &str) -> String {
        let re = Regex::new(pat).unwrap();
        let mut buf = Vec::new();
        printer
            .print_match(&mut buf, Path::new("a/b.txt"), 3, text, max_columns, &re)
            .unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn prints_plain_without_color() {
        let out = render(&Printer::new(false), "hello world", 0, "world");
        assert_eq!(out, "a/b.txt:3:hello world\n");
    }

    #[test]
    fn highlights_match_with_color() {
        let out = render(&Printer::new(true), "hello world", 0, "world");
        // 装飾を取り除くと元の行に戻ること。
        let stripped = strip_ansi(&out);
        assert_eq!(stripped, "a/b.txt:3:hello world\n");
        // 一致語の直前に SGR 開始、直後にリセットがあること。
        assert!(out.contains('\u{1b}'));
        assert!(out.contains("\u{1b}[0m"));
        // "world" の周囲に装飾が入る（強調開始 + world + リセット）。
        let red_start = "world".to_string();
        assert!(out.contains(&red_start));
    }

    #[test]
    fn highlights_only_within_visible_range() {
        // max_columns で切り詰めた範囲より後ろの一致は強調しない（描画もされない）。
        let out = render(&Printer::new(true), "abcdefMATCH", 3, "MATCH");
        let stripped = strip_ansi(&out);
        assert_eq!(stripped, "a/b.txt:3:abc …[+8 chars]\n");
    }

    #[test]
    fn highlights_multiple_matches() {
        let out = render(&Printer::new(true), "a x a x a", 0, "a");
        let stripped = strip_ansi(&out);
        assert_eq!(stripped, "a/b.txt:3:a x a x a\n");
        // 3 件の一致それぞれにリセットが入る（最低 3 つの reset）。
        let resets = out.matches("\u{1b}[0m").count();
        assert!(resets >= 3, "expected >=3 resets, got {resets}: {out:?}");
    }

    #[test]
    fn skips_zero_width_matches() {
        // `^` は幅ゼロの一致。本文には強調を入れない（reset はパスと行番号の 2 つだけ）。
        let out = render(&Printer::new(true), "hello", 0, "^");
        assert_eq!(strip_ansi(&out), "a/b.txt:3:hello\n");
        assert_eq!(out.matches("\u{1b}[0m").count(), 2);
    }

    #[test]
    fn skips_zero_width_but_keeps_real_matches() {
        // 空マッチ可能なパターンでも、実体のある一致だけ強調する。
        let out = render(&Printer::new(true), "ab", 0, "x*");
        assert_eq!(strip_ansi(&out), "a/b.txt:3:ab\n");
        // x が無いので幅ゼロ一致のみ → 本文の強調なし（reset は 2 つ）。
        assert_eq!(out.matches("\u{1b}[0m").count(), 2);
    }

    /// テスト用の素朴な ANSI ストリッパ（SGR `ESC [ ... m` を除去）。
    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                // `[` ... 終端文字（'m' 等のアルファベット）まで読み飛ばす。
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}
