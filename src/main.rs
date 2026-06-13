use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use clap::Parser;
use ignore::Walk;
use regex::Regex;

/// 高速ローカル検索 CLI（最小版）
#[derive(Parser, Debug)]
#[command(name = "fastgrep-rs", version, about = "Fast local search CLI")]
struct Cli {
    /// 検索する正規表現パターン
    pattern: String,

    /// 検索対象のパス（ファイル or ディレクトリ）
    #[arg(default_value = ".")]
    path: String,
}

/// 1 件のマッチ結果。
#[derive(Debug, PartialEq, Eq)]
struct Match {
    line_number: usize,
    line: String,
}

/// テキスト内容を行単位で検索し、マッチした行を返す。
fn search_in_content(content: &str, re: &Regex) -> Vec<Match> {
    content
        .lines()
        .enumerate()
        .filter(|(_, line)| re.is_match(line))
        .map(|(idx, line)| Match {
            line_number: idx + 1,
            line: line.to_string(),
        })
        .collect()
}

/// 単一ファイルを読み込んで検索し、`path:line_number:line` 形式で出力する。
///
/// UTF-8 として読めないファイル（バイナリ等）は黙ってスキップする。
fn search_file(path: &Path, re: &Regex) -> Result<()> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        // UTF-8 でないファイルはスキップ
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => return Ok(()),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read {}", path.display()));
        }
    };

    for m in search_in_content(&content, re) {
        println!("{}:{}:{}", path.display(), m.line_number, m.line);
    }
    Ok(())
}

fn run(cli: &Cli) -> Result<()> {
    let re = Regex::new(&cli.pattern)
        .with_context(|| format!("invalid regex pattern: {}", cli.pattern))?;

    for result in Walk::new(&cli.path) {
        let entry = result.with_context(|| "failed to walk directory")?;
        // ディレクトリ等はスキップし、通常ファイルのみ対象にする。
        if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            search_file(entry.path(), &re)?;
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

    #[test]
    fn matches_single_line() {
        let re = Regex::new("world").unwrap();
        let content = "hello\nworld\nfoo";
        let matches = search_in_content(content, &re);
        assert_eq!(
            matches,
            vec![Match {
                line_number: 2,
                line: "world".to_string(),
            }]
        );
    }

    #[test]
    fn matches_multiple_lines_with_regex() {
        let re = Regex::new(r"第[0-9]+条").unwrap();
        let content = "前文\n第1条 著作権\n説明\n第12条 範囲";
        let matches = search_in_content(content, &re);
        assert_eq!(
            matches,
            vec![
                Match {
                    line_number: 2,
                    line: "第1条 著作権".to_string(),
                },
                Match {
                    line_number: 4,
                    line: "第12条 範囲".to_string(),
                },
            ]
        );
    }

    #[test]
    fn no_match_returns_empty() {
        let re = Regex::new("missing").unwrap();
        let matches = search_in_content("a\nb\nc", &re);
        assert!(matches.is_empty());
    }
}
