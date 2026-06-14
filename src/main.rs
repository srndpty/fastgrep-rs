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
use crate::search::{SearchError, search_file};
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
    // 未指定時: 端末なら端末幅に合わせ、パイプ時は切り詰めない（下流処理を壊さない）。
    let max_columns = match cli.max_columns {
        Some(n) => n,
        None if stdout_is_tty => terminal_width().unwrap_or(0),
        None => 0,
    };

    // Auto は端末判定に加えて NO_COLOR / CLICOLOR / dumb 端末を尊重して解決する。
    let color_choice = match cli.color {
        ColorWhen::Auto => anstream::AutoStream::choice(&io::stdout()),
        ColorWhen::Always => anstream::ColorChoice::Always,
        ColorWhen::Never => anstream::ColorChoice::Never,
    };
    let use_color = !matches!(color_choice, anstream::ColorChoice::Never);
    let printer = Printer::new(use_color);

    // 色 ON のときだけ AutoStream を通す（スタイルを通し、Windows では VT を有効化）。
    // 色 OFF で AutoStream(Never) に通すと、ファイル内容中の正当な ANSI まで
    // ストリップして出力を壊すため、素の writer に書く（Printer もスタイルを出さない）。
    let mut out: Box<dyn Write> = if use_color {
        Box::new(BufWriter::new(anstream::AutoStream::new(
            io::stdout().lock(),
            color_choice,
        )))
    } else {
        Box::new(BufWriter::new(io::stdout().lock()))
    };

    // 指定パスの stat 自体に失敗（存在しない等）する場合は fatal にする。
    let meta = cli
        .path
        .metadata()
        .with_context(|| format!("cannot access path: {}", cli.path.display()))?;
    // 要求されたルートがディレクトリなら、一覧できることも確認する。一覧権限のない
    // ディレクトリは ignore::Walk のエラーの depth に依存せず、ここで fatal にする。
    if meta.is_dir() {
        std::fs::read_dir(&cli.path)
            .with_context(|| format!("cannot read directory: {}", cli.path.display()))?;
    }

    for result in Walk::new(&cli.path) {
        // ルートの可読性は上で検証済みなので、走査途中のエラーは子孫の問題として継続。
        let entry = match result {
            Ok(entry) => entry,
            Err(e) => {
                eprintln!("warning: {e}");
                continue;
            }
        };
        // 要求されたルートそのもの（depth 0）か。ファイルを直接指定した場合に該当。
        let is_root = entry.depth() == 0;
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
        match search_file(entry.path(), &re, max_columns, &printer, &mut out) {
            Ok(()) => {}
            // 要求されたルートのファイル自体が読めない場合は fatal（何も検索できない）。
            Err(SearchError::Read(e)) if is_root => {
                return Err(e).context("failed to read requested path");
            }
            // 子孫ファイルの読み取りエラーは全体を止めず、警告して継続する。
            Err(SearchError::Read(e)) => eprintln!("warning: {e:#}"),
            // 出力先が閉じられた（`... | head` 等）場合は静かに終了する。
            Err(SearchError::Write(e)) if e.kind() == ErrorKind::BrokenPipe => return Ok(()),
            // それ以外の書き込み失敗（EIO/ENOSPC 等）は結果が不完全になるため abort する。
            Err(SearchError::Write(e)) => return Err(e).context("failed to write output"),
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    run(&cli)
}
