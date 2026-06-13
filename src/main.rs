//! fastgrep-rs: 高速ローカル検索 CLI。
//!
//! CLI 定義と全体の配線のみを担当し、検索ロジックは各モジュールに分離している。

mod output;
mod search;
mod size;

use std::io::{self, BufWriter, ErrorKind, IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use ignore::Walk;
use regex::RegexBuilder;

use crate::output::Printer;
use crate::search::search_file;
use crate::size::parse_size;

/// 色付けの方針。
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
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

    /// パス・行番号の色付け（本文中の一致箇所はハイライトしない）
    #[arg(long, value_enum, default_value_t = ColorWhen::Auto)]
    color: ColorWhen,

    /// 出力する本文の最大「文字数」（表示幅ではない。0 で無制限）。
    /// 未指定なら端末では端末幅を目安に切り詰め、パイプ時は無制限。
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

    // 指定パスの stat 自体に失敗（存在しない等）する場合は fatal にする。
    cli.path
        .metadata()
        .with_context(|| format!("cannot access path: {}", cli.path.display()))?;

    for result in Walk::new(&cli.path) {
        let entry = match result {
            Ok(entry) => entry,
            Err(e) => {
                // ルート（depth 0）の走査失敗（例: 一覧権限のないディレクトリ）は
                // 何も検索できないので fatal。子孫のエラーは警告して継続する。
                if e.depth() == Some(0) {
                    return Err(e).with_context(|| {
                        format!("failed to traverse path: {}", cli.path.display())
                    });
                }
                eprintln!("warning: {e}");
                continue;
            }
        };
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
        if let Err(e) = search_file(entry.path(), &re, max_columns, &printer, &mut out) {
            // `... | head` 等で出力先が閉じられた場合は静かに終了する。
            if is_broken_pipe(&e) {
                return Ok(());
            }
            // 1 ファイルの読み取りエラーは全体を止めず、警告して継続する。
            eprintln!("warning: {e:#}");
        }
    }

    // 出力先が既に閉じられている場合（BrokenPipe）はエラー扱いしない。
    if let Err(e) = out.flush()
        && e.kind() != ErrorKind::BrokenPipe
    {
        return Err(e).context("failed to flush output");
    }
    Ok(())
}

/// エラーチェイン内に `BrokenPipe` の I/O エラーが含まれるか。
fn is_broken_pipe(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|io_err| io_err.kind() == ErrorKind::BrokenPipe)
    })
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    run(&cli)
}
