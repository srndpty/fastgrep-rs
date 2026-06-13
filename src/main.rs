use std::fs::File;
use std::io::{self, BufRead, BufReader, ErrorKind};
use std::path::Path;

use anyhow::{Context, Result};
use clap::Parser;
use ignore::Walk;
use regex::Regex;

/// バイナリ判定のために先頭から読むバイト数。
const BINARY_SNIFF_BYTES: usize = 8 * 1024;
/// 1 行の最大バイト数。これを超える行（巨大データの 1 行等）はスキップする。
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// 高速ローカル検索 CLI（最小版）
#[derive(Parser, Debug)]
#[command(name = "fastgrep-rs", version, about = "Fast local search CLI")]
struct Cli {
    /// 検索する正規表現パターン
    pattern: String,

    /// 検索対象のパス（ファイル or ディレクトリ）
    #[arg(default_value = ".")]
    path: String,

    /// 出力する 1 行の最大文字数。超過分は省略表示する（0 で無制限）
    #[arg(long, default_value_t = 250)]
    max_columns: usize,

    /// この大きさを超えるファイルはスキップ（例: 10M, 500K, 0=無制限）
    #[arg(long, default_value = "10M")]
    max_filesize: String,
}

/// `10M` / `500K` / `2G` / `1024` のようなサイズ文字列をバイト数に変換する。
fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    anyhow::ensure!(!s.is_empty(), "empty size");

    let last = s.chars().last().unwrap();
    let (num, mult): (&str, u64) = match last.to_ascii_uppercase() {
        'K' => (&s[..s.len() - 1], 1024),
        'M' => (&s[..s.len() - 1], 1024 * 1024),
        'G' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    let n: u64 = num
        .trim()
        .parse()
        .with_context(|| format!("invalid size value: {s}"))?;
    Ok(n * mult)
}

/// 表示用に行を max_columns 文字までに切り詰める（0 なら無制限）。
fn truncate_for_display(line: &str, max_columns: usize) -> String {
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

/// reader から 1 行を最大 cap バイトまで読む。
///
/// 戻り値:
/// - `Ok(None)`         … EOF（もう行がない）
/// - `Ok(Some(true))`   … cap を超えた行（buf は途中まで／信頼できない）
/// - `Ok(Some(false))`  … 通常の行（buf に内容、末尾改行は含まない）
///
/// cap を超えた場合でもファイルは改行までバッファ単位で読み飛ばすため、
/// 「ファイル全体が 1 行」でも一気にメモリへ載せない。
fn read_capped_line<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    cap: usize,
) -> io::Result<Option<bool>> {
    buf.clear();
    let mut read_any = false;
    let mut oversized = false;

    loop {
        let available = match reader.fill_buf() {
            Ok(b) => b,
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            break; // EOF
        }
        read_any = true;

        if let Some(i) = available.iter().position(|&b| b == b'\n') {
            if !oversized && buf.len() + i <= cap {
                buf.extend_from_slice(&available[..i]);
            } else {
                oversized = true;
            }
            reader.consume(i + 1);
            break;
        } else {
            let take = available.len();
            if !oversized && buf.len() + take <= cap {
                buf.extend_from_slice(available);
            } else {
                oversized = true;
            }
            reader.consume(take);
        }
    }

    if read_any {
        Ok(Some(oversized))
    } else {
        Ok(None)
    }
}

/// 単一ファイルをストリーミング検索し、`path:line_number:line` 形式で出力する。
///
/// - 先頭に NUL を含むファイル（バイナリ）はスキップ
/// - UTF-8 でない行・超長行はスキップ
fn search_file(path: &Path, re: &Regex, max_columns: usize) -> Result<()> {
    let file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut reader = BufReader::new(file);

    // バイナリ判定: 先頭チャンクに NUL があればスキップ。
    {
        let head = reader
            .fill_buf()
            .with_context(|| format!("failed to read {}", path.display()))?;
        let sniff = &head[..head.len().min(BINARY_SNIFF_BYTES)];
        if sniff.contains(&0) {
            return Ok(());
        }
    }

    let mut line_no = 0usize;
    let mut buf: Vec<u8> = Vec::new();
    while let Some(oversized) = read_capped_line(&mut reader, &mut buf, MAX_LINE_BYTES)? {
        line_no += 1;
        if oversized {
            continue; // 巨大な 1 行（データ等）は検索対象外
        }
        // UTF-8 でない行（バイナリ混入など）はスキップ。
        let Ok(text) = std::str::from_utf8(&buf) else {
            continue;
        };
        let text = text.trim_end_matches(['\n', '\r']);
        if re.is_match(text) {
            println!(
                "{}:{}:{}",
                path.display(),
                line_no,
                truncate_for_display(text, max_columns)
            );
        }
    }
    Ok(())
}

fn run(cli: &Cli) -> Result<()> {
    let re = Regex::new(&cli.pattern)
        .with_context(|| format!("invalid regex pattern: {}", cli.pattern))?;
    let max_filesize = parse_size(&cli.max_filesize)?;

    for result in Walk::new(&cli.path) {
        let entry = result.context("failed to walk directory")?;
        // 通常ファイルのみ対象にする。
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        // サイズ上限を超えるファイルはスキップ。
        if max_filesize > 0 {
            if let Ok(meta) = entry.metadata() {
                if meta.len() > max_filesize {
                    continue;
                }
            }
        }
        // 1 ファイルのエラーで全体を止めず、警告して継続する。
        if let Err(e) = search_file(entry.path(), &re, cli.max_columns) {
            eprintln!("warning: {e:#}");
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    run(&cli)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テキスト内容を行単位で検索し、マッチした (行番号, 行) を返す（テスト用）。
    fn search_in_content(content: &str, re: &Regex) -> Vec<(usize, String)> {
        content
            .lines()
            .enumerate()
            .filter(|(_, line)| re.is_match(line))
            .map(|(idx, line)| (idx + 1, line.to_string()))
            .collect()
    }

    #[test]
    fn matches_single_line() {
        let re = Regex::new("world").unwrap();
        let matches = search_in_content("hello\nworld\nfoo", &re);
        assert_eq!(matches, vec![(2, "world".to_string())]);
    }

    #[test]
    fn matches_multiple_lines_with_regex() {
        let re = Regex::new(r"第[0-9]+条").unwrap();
        let matches = search_in_content("前文\n第1条 著作権\n説明\n第12条 範囲", &re);
        assert_eq!(
            matches,
            vec![
                (2, "第1条 著作権".to_string()),
                (4, "第12条 範囲".to_string()),
            ]
        );
    }

    #[test]
    fn no_match_returns_empty() {
        let re = Regex::new("missing").unwrap();
        assert!(search_in_content("a\nb\nc", &re).is_empty());
    }

    #[test]
    fn parse_size_units() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("500K").unwrap(), 500 * 1024);
        assert_eq!(parse_size("10M").unwrap(), 10 * 1024 * 1024);
        assert_eq!(parse_size("2g").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("0").unwrap(), 0);
        assert!(parse_size("abc").is_err());
        assert!(parse_size("").is_err());
    }

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
    fn read_capped_line_reads_lines_and_eof() {
        let data = b"abc\ndef\n".to_vec();
        let mut reader = BufReader::new(&data[..]);
        let mut buf = Vec::new();

        assert_eq!(read_capped_line(&mut reader, &mut buf, 1024).unwrap(), Some(false));
        assert_eq!(&buf, b"abc");
        assert_eq!(read_capped_line(&mut reader, &mut buf, 1024).unwrap(), Some(false));
        assert_eq!(&buf, b"def");
        assert_eq!(read_capped_line(&mut reader, &mut buf, 1024).unwrap(), None);
    }

    #[test]
    fn read_capped_line_flags_oversized() {
        // cap=4 に対して長い 1 行 → oversized=true、次は EOF。
        let data = b"0123456789\n".to_vec();
        let mut reader = BufReader::new(&data[..]);
        let mut buf = Vec::new();

        assert_eq!(read_capped_line(&mut reader, &mut buf, 4).unwrap(), Some(true));
        assert_eq!(read_capped_line(&mut reader, &mut buf, 4).unwrap(), None);
    }
}
