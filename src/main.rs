use std::fs::File;
use std::io::{self, BufRead, BufReader, ErrorKind};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use ignore::Walk;
use regex::{Regex, RegexBuilder};

/// バイナリ判定のために先頭バッファから最大このバイト数だけ見る。
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
    path: PathBuf,

    /// 大文字・小文字を区別しない
    #[arg(short = 'i', long)]
    ignore_case: bool,

    /// 出力する 1 行の最大文字数。超過分は省略表示する（0 で無制限）
    #[arg(long, default_value_t = 250)]
    max_columns: usize,

    /// この大きさを超えるファイルはスキップ（例: 10M, 500K, 10MB, 0=無制限）
    #[arg(long, default_value = "10M")]
    max_filesize: String,
}

/// `10M` / `500K` / `2G` / `10MB` / `1024` のようなサイズ文字列をバイト数に変換する。
///
/// 接尾辞は大文字小文字を区別せず、`K`/`KB`/`M`/`MB`/`G`/`GB` を受け付ける。
fn parse_size(s: &str) -> Result<u64> {
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
/// - `Ok(Some(false))`  … 通常の行（buf に内容。行末の LF は含まない。CRLF の
///   CR は残るため、利用側で trim する想定）
///
/// cap を超えた場合でもファイルは改行までバッファ単位で読み飛ばすため、
/// 「ファイル全体が 1 行」でも一気にメモリへ載せない。
///
/// `cap` は 1 以上を前提とする（`cap == 0` だと空行以外すべて oversized 扱い）。
fn read_capped_line<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    cap: usize,
) -> io::Result<Option<bool>> {
    debug_assert!(cap > 0, "cap must be > 0");
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

/// reader をストリーミング検索し、マッチした行ごとに `on_match(line_no, text)` を呼ぶ。
///
/// 出力や整形は呼び出し側に委ねる（`--json` 等への拡張のため、検索と出力を分離）。
/// マッチを Vec に溜めず逐次コールバックするため、メモリ使用量は行サイズに比例して安定する。
///
/// - UTF-8 でない行・超長行（>`MAX_LINE_BYTES`）はスキップ
fn search_reader<R: BufRead>(
    reader: &mut R,
    re: &Regex,
    mut on_match: impl FnMut(usize, &str),
) -> io::Result<()> {
    let mut line_no = 0usize;
    let mut buf: Vec<u8> = Vec::new();
    while let Some(oversized) = read_capped_line(reader, &mut buf, MAX_LINE_BYTES)? {
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
            on_match(line_no, text);
        }
    }
    Ok(())
}

/// 単一ファイルをストリーミング検索し、`path:line_number:line` 形式で出力する。
///
/// - 先頭に NUL を含むファイル（バイナリ）はスキップ
fn search_file(path: &Path, re: &Regex, max_columns: usize) -> Result<()> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut reader = BufReader::new(file);

    // バイナリ判定: 先頭バッファに NUL があればスキップ。
    {
        let head = reader
            .fill_buf()
            .with_context(|| format!("failed to read {}", path.display()))?;
        let sniff = &head[..head.len().min(BINARY_SNIFF_BYTES)];
        if sniff.contains(&0) {
            return Ok(());
        }
    }

    search_reader(&mut reader, re, |line_no, text| {
        println!(
            "{}:{}:{}",
            path.display(),
            line_no,
            truncate_for_display(text, max_columns)
        );
    })
    .with_context(|| format!("failed to read {}", path.display()))
}

fn run(cli: &Cli) -> Result<()> {
    let re = RegexBuilder::new(&cli.pattern)
        .case_insensitive(cli.ignore_case)
        .build()
        .with_context(|| format!("invalid regex pattern: {}", cli.pattern))?;
    let max_filesize = parse_size(&cli.max_filesize)?;

    for result in Walk::new(&cli.path) {
        let entry = result.context("failed to walk directory")?;
        // 通常ファイルのみ対象にする。
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        // サイズ上限を超えるファイルはスキップ。
        // metadata が取れない場合はサイズ判定せず、読み取りを試みる。
        if max_filesize > 0
            && let Ok(meta) = entry.metadata()
            && meta.len() > max_filesize
        {
            continue;
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

    /// 本番経路（search_reader + read_capped_line + from_utf8 + trim）を通して
    /// マッチした (行番号, 行) を集める。バイト列をそのまま入力できる。
    fn collect_matches(content: &[u8], re: &Regex) -> Vec<(usize, String)> {
        let mut reader = BufReader::new(content);
        let mut out = Vec::new();
        search_reader(&mut reader, re, |line_no, text| {
            out.push((line_no, text.to_string()))
        })
        .unwrap();
        out
    }

    #[test]
    fn matches_single_line() {
        let re = Regex::new("world").unwrap();
        let matches = collect_matches(b"hello\nworld\nfoo", &re);
        assert_eq!(matches, vec![(2, "world".to_string())]);
    }

    #[test]
    fn matches_multiple_lines_with_regex() {
        let re = Regex::new(r"第[0-9]+条").unwrap();
        let matches = collect_matches("前文\n第1条 著作権\n説明\n第12条 範囲".as_bytes(), &re);
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
        assert!(collect_matches(b"a\nb\nc", &re).is_empty());
    }

    #[test]
    fn strips_crlf_before_matching() {
        // CRLF でも CR を含まずにマッチ・取得できること。
        let re = Regex::new("^world$").unwrap();
        let matches = collect_matches(b"hello\r\nworld\r\n", &re);
        assert_eq!(matches, vec![(2, "world".to_string())]);
    }

    #[test]
    fn skips_oversized_line() {
        // MAX_LINE_BYTES を超える 1 行は検索対象外。前後の通常行は拾う。
        let big = "x".repeat(MAX_LINE_BYTES + 10);
        let content = format!("hit\n{big}\nhit\n");
        let re = Regex::new("hit").unwrap();
        let matches = collect_matches(content.as_bytes(), &re);
        assert_eq!(
            matches,
            vec![(1, "hit".to_string()), (3, "hit".to_string())]
        );
    }

    #[test]
    fn case_insensitive_via_builder() {
        let re = RegexBuilder::new("todo")
            .case_insensitive(true)
            .build()
            .unwrap();
        let matches = collect_matches(b"TODO: x\nnope\n", &re);
        assert_eq!(matches, vec![(1, "TODO: x".to_string())]);
    }

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

        assert_eq!(
            read_capped_line(&mut reader, &mut buf, 1024).unwrap(),
            Some(false)
        );
        assert_eq!(&buf, b"abc");
        assert_eq!(
            read_capped_line(&mut reader, &mut buf, 1024).unwrap(),
            Some(false)
        );
        assert_eq!(&buf, b"def");
        assert_eq!(read_capped_line(&mut reader, &mut buf, 1024).unwrap(), None);
    }

    #[test]
    fn read_capped_line_flags_oversized() {
        // cap=4 に対して長い 1 行 → oversized=true、次は EOF。
        let data = b"0123456789\n".to_vec();
        let mut reader = BufReader::new(&data[..]);
        let mut buf = Vec::new();

        assert_eq!(
            read_capped_line(&mut reader, &mut buf, 4).unwrap(),
            Some(true)
        );
        assert_eq!(read_capped_line(&mut reader, &mut buf, 4).unwrap(), None);
    }
}
