//! ファイル走査・ストリーミング読み取り・行マッチング。

use std::fs::File;
use std::io::{self, BufRead, BufReader, ErrorKind, Write};
use std::path::Path;

use anyhow::Context;
use regex::Regex;

use crate::output::Printer;

/// バイナリ判定のために先頭バッファから最大このバイト数だけ見る。
const BINARY_SNIFF_BYTES: usize = 8 * 1024;
/// 1 行の最大バイト数。これを超える行（巨大データの 1 行等）はスキップする。
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// 検索処理の失敗理由。
///
/// 読み取り側と書き込み側を区別し、呼び出し側で扱いを変えられるようにする。
/// - `Read`: 対象ファイルの読み取り失敗。1 ファイルの問題なので警告して継続できる。
/// - `Write`: 出力先への書き込み失敗。結果が不完全になるため致命的（ただし
///   `BrokenPipe` は呼び出し側で静かに終了する）。
#[derive(Debug)]
pub enum SearchError {
    Read(anyhow::Error),
    Write(io::Error),
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
pub fn read_capped_line<R: BufRead>(
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
/// マッチを溜め込まず逐次コールバックするため、メモリ使用量は行サイズに比例して安定する。
///
/// reader からの読み取り失敗は [`SearchError::Read`]、`on_match`（出力）の失敗は
/// [`SearchError::Write`] として区別して返す。
///
/// - UTF-8 でない行・超長行（>`MAX_LINE_BYTES`）はスキップ
pub fn search_reader<R: BufRead>(
    reader: &mut R,
    re: &Regex,
    mut on_match: impl FnMut(usize, &str) -> io::Result<()>,
) -> Result<(), SearchError> {
    let mut line_no = 0usize;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let oversized = match read_capped_line(reader, &mut buf, MAX_LINE_BYTES) {
            Ok(Some(oversized)) => oversized,
            Ok(None) => break,
            Err(e) => return Err(SearchError::Read(e.into())),
        };
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
            on_match(line_no, text).map_err(SearchError::Write)?;
        }
    }
    Ok(())
}

/// 単一ファイルをストリーミング検索し、`printer` 経由で `out` に出力する。
///
/// - 先頭に NUL を含むファイル（バイナリ）はスキップ
pub fn search_file<W: Write>(
    path: &Path,
    re: &Regex,
    max_columns: usize,
    printer: &Printer,
    out: &mut W,
) -> Result<(), SearchError> {
    let file = File::open(path)
        .with_context(|| format!("failed to open {}", path.display()))
        .map_err(SearchError::Read)?;
    let mut reader = BufReader::new(file);

    // バイナリ判定: 先頭バッファに NUL があればスキップ。
    {
        let head = reader
            .fill_buf()
            .with_context(|| format!("failed to read {}", path.display()))
            .map_err(SearchError::Read)?;
        let sniff = &head[..head.len().min(BINARY_SNIFF_BYTES)];
        if sniff.contains(&0) {
            return Ok(());
        }
    }

    search_reader(&mut reader, re, |line_no, text| {
        printer.print_match(out, path, line_no, text, max_columns, re)
    })
    .map_err(|e| match e {
        // 読み取りエラーにはファイルパスの文脈を付ける。書き込みエラーはそのまま。
        SearchError::Read(err) => {
            SearchError::Read(err.context(format!("failed to read {}", path.display())))
        }
        SearchError::Write(err) => SearchError::Write(err),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::RegexBuilder;

    /// 本番経路（search_reader + read_capped_line + from_utf8 + trim）を通して
    /// マッチした (行番号, 行) を集める。バイト列をそのまま入力できる。
    fn collect_matches(content: &[u8], re: &Regex) -> Vec<(usize, String)> {
        let mut reader = BufReader::new(content);
        let mut out = Vec::new();
        search_reader(&mut reader, re, |line_no, text| {
            out.push((line_no, text.to_string()));
            Ok(())
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
