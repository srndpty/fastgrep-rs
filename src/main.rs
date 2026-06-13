//! fastgrep-rs: 高速ローカル検索 CLI。
//!
//! CLI 定義と全体の配線のみを担当し、検索ロジックは各モジュールに分離している。

mod output;
mod search;
mod size;

use std::io::{self, BufWriter, IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use ignore::Walk;
use regex::RegexBuilder;

use crate::output::Printer;
use crate::search::search_file;
use crate::size::parse_size;

/// 色付けの方針。
#[derive(Copy, Clone, Debug, ValueEnum)]
enum ColorWhen {
    /// 端末出力のときだけ色を付ける（パイプ時は無効）
    Auto,
    /// 常に色を付ける
    Always,
    /// 色を付けない
    Never,
}

/// 高速ローカル検索 CLI
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

    /// 色付け
    #[arg(long, value_enum, default_value_t = ColorWhen::Auto)]
    color: ColorWhen,

    /// 出力する 1 行の最大文字数（0 で無制限）。
    /// 未指定なら端末では端末幅、パイプ時は無制限。
    #[arg(long)]
    max_columns: Option<usize>,

    /// この大きさを超えるファイルはスキップ（例: 10M, 500K, 10MB, 0=無制限）
    #[arg(long, default_value = "10M")]
    max_filesize: String,
}

/// 端末の桁数を取得する（不明なら None）。
fn terminal_width() -> Option<usize> {
    terminal_size::terminal_size().map(|(w, _h)| w.0 as usize)
}

fn run(cli: &Cli) -> Result<()> {
    let re = RegexBuilder::new(&cli.pattern)
        .case_insensitive(cli.ignore_case)
        .build()
        .with_context(|| format!("invalid regex pattern: {}", cli.pattern))?;
    let max_filesize = parse_size(&cli.max_filesize)?;

    let stdout_is_tty = io::stdout().is_terminal();
    let use_color = match cli.color {
        ColorWhen::Always => true,
        ColorWhen::Never => false,
        ColorWhen::Auto => stdout_is_tty,
    };
    // 未指定時: 端末なら端末幅に合わせ、パイプ時は切り詰めない（下流処理を壊さない）。
    let max_columns = match cli.max_columns {
        Some(n) => n,
        None if stdout_is_tty => terminal_width().unwrap_or(0),
        None => 0,
    };

    // anstream に Windows の VT 有効化と非対応時のストリップを任せる。
    let choice = if use_color {
        anstream::ColorChoice::Always
    } else {
        anstream::ColorChoice::Never
    };
    let printer = Printer::new(use_color);
    let mut out = BufWriter::new(anstream::AutoStream::new(io::stdout().lock(), choice));

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
        if let Err(e) = search_file(entry.path(), &re, max_columns, &printer, &mut out) {
            eprintln!("warning: {e:#}");
        }
    }
    out.flush()?;
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    run(&cli)
}
