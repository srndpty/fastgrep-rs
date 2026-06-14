//! fastgrep-rs: 高速ローカル検索 CLI。
//!
//! CLI 定義と全体の配線のみを担当し、検索ロジックは各モジュールに分離している。

mod output;
mod search;
mod size;

use std::io::{self, BufWriter, ErrorKind, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use ignore::{DirEntry, Walk};
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};

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
    #[arg(long, default_value = "1M")]
    max_filesize: String,

    /// 検索に使うスレッド数（0=自動=論理コア数、1=逐次）。
    /// 2 以上の並列時はファイル単位で出力がまとまるが、ファイル間の順序は非決定的。
    #[arg(short = 'j', long, default_value_t = 0)]
    threads: usize,
}

/// 並列検索を打ち切る理由。
enum Stop {
    /// 出力先が閉じられた（`... | head` 等）。静かに正常終了する。
    BrokenPipe,
    /// 致命的エラー。検索全体を中断する。
    Fatal(anyhow::Error),
}

/// 端末の桁数を取得する（不明なら None）。
fn terminal_width() -> Option<usize> {
    terminal_size::terminal_size().map(|(w, _h)| w.0 as usize)
}

/// このエントリを検索対象にするか（通常ファイルかつサイズ上限内か）。
fn should_search(entry: &DirEntry, max_filesize: u64) -> bool {
    if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
        return false;
    }
    // metadata が取れない場合はサイズ判定せず、読み取りを試みる。
    if max_filesize > 0
        && let Ok(meta) = entry.metadata()
        && meta.len() > max_filesize
    {
        return false;
    }
    true
}

/// 逐次検索（`-j 1`）。走査と検索を交互に行い、マッチを即時に出力する。
fn search_sequential<W: Write>(
    root: &Path,
    re: &Regex,
    max_columns: usize,
    max_filesize: u64,
    printer: &Printer,
    out: &mut W,
) -> Result<()> {
    for result in Walk::new(root) {
        // ルートの可読性は呼び出し前に検証済みなので、走査途中のエラーは子孫の問題。
        let entry = match result {
            Ok(entry) => entry,
            Err(e) => {
                eprintln!("warning: {e}");
                continue;
            }
        };
        let is_root = entry.depth() == 0;
        if !should_search(&entry, max_filesize) {
            continue;
        }
        match search_file(entry.path(), re, max_columns, printer, out) {
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
    Ok(())
}

/// 並列検索（`-j` が 2 以上）。各ファイルを個別バッファに検索し、`writer` へ
/// 排他的に一括書き込みする。ファイル単位で出力はまとまるが、順序は非決定的。
fn search_parallel(
    root: &Path,
    re: &Regex,
    max_columns: usize,
    max_filesize: u64,
    printer: &Printer,
    writer: &Mutex<Box<dyn Write + Send>>,
) -> Result<(), Stop> {
    // 候補ファイルを収集する。走査自体は逐次で、重いファイル検索を並列化する。
    let mut targets: Vec<(PathBuf, bool)> = Vec::new();
    for result in Walk::new(root) {
        match result {
            Ok(entry) => {
                let is_root = entry.depth() == 0;
                if should_search(&entry, max_filesize) {
                    targets.push((entry.into_path(), is_root));
                }
            }
            Err(e) => eprintln!("warning: {e}"),
        }
    }

    targets.par_iter().try_for_each(|(path, is_root)| {
        // 出力はいったんファイル単位のバッファに書く（Vec への書き込みは失敗しない）。
        let mut buf: Vec<u8> = Vec::new();
        match search_file(path, re, max_columns, printer, &mut buf) {
            Ok(()) => {}
            Err(SearchError::Read(e)) if *is_root => {
                return Err(Stop::Fatal(e.context("failed to read requested path")));
            }
            Err(SearchError::Read(e)) => {
                eprintln!("warning: {e:#}");
                return Ok(());
            }
            Err(SearchError::Write(e)) => return Err(Stop::Fatal(anyhow::Error::new(e))),
        }
        if buf.is_empty() {
            return Ok(());
        }
        // 1 ファイル分をまとめて書くことで、並列でも行が混ざらない。
        let mut w = writer.lock().expect("writer mutex poisoned");
        match w.write_all(&buf) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::BrokenPipe => Err(Stop::BrokenPipe),
            Err(e) => Err(Stop::Fatal(
                anyhow::Error::new(e).context("failed to write output"),
            )),
        }
    })
}

fn run(cli: &Cli) -> Result<()> {
    let re = RegexBuilder::new(&cli.pattern)
        .case_insensitive(cli.ignore_case)
        .build()
        .with_context(|| format!("invalid regex pattern: {}", cli.pattern))?;
    let max_filesize = parse_size(&cli.max_filesize)?;
    let threads = match cli.threads {
        0 => std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        n => n,
    };

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
    // 並列スレッドから共有するため、Send な io::stdout() を土台にする。
    let styled: Box<dyn Write + Send> = if use_color {
        Box::new(anstream::AutoStream::new(io::stdout(), color_choice))
    } else {
        Box::new(io::stdout())
    };
    // 端末では std の行バッファ（改行ごとに flush）に任せ、まばらなマッチも即時表示する。
    // 非端末（パイプ/リダイレクト）ではスループットのためブロックバッファを足す。
    let out: Box<dyn Write + Send> = if stdout_is_tty {
        styled
    } else {
        Box::new(BufWriter::new(styled))
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

    if threads <= 1 {
        // 逐次: 即時・決定的な出力。
        let mut out = out;
        search_sequential(
            &cli.path,
            &re,
            max_columns,
            max_filesize,
            &printer,
            &mut out,
        )?;
        flush_output(&mut out)?;
        Ok(())
    } else {
        // 並列: 専用スレッドプールでファイル検索を分散する。
        let writer = Mutex::new(out);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .context("failed to build thread pool")?;
        let outcome = pool.install(|| {
            search_parallel(&cli.path, &re, max_columns, max_filesize, &printer, &writer)
        });
        let mut out = writer.into_inner().expect("writer mutex poisoned");
        flush_output(&mut out)?;
        match outcome {
            Ok(()) | Err(Stop::BrokenPipe) => Ok(()),
            Err(Stop::Fatal(e)) => Err(e),
        }
    }
}

/// 出力をフラッシュする。出力先が閉じられている（BrokenPipe）場合はエラー扱いしない。
fn flush_output<W: Write>(out: &mut W) -> Result<()> {
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
