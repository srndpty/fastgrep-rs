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
    /// 2 以上の並列時は順序が非決定的。出力の小さいファイルはまとまって出るが、
    /// 出力が大きいファイルはチャンク分割され他ファイルと行が交互に出ることがある
    /// （各行にパス接頭辞があり判別可能）。
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

/// 並列時に 1 ファイルの出力を全部メモリに溜めず、しきい値を超えたら共有 writer へ
/// 「最後の改行まで」をまとめて書き出すラッパ。
///
/// 行の途中では書き出さないので、各行は壊れずに出力される。出力が小さいファイルは
/// 末尾の `flush` で 1 回だけ書かれて連続するが、大きいファイルはチャンク分割され、
/// 並列時は他ファイルと行が交互に出ることがある（各行にパス接頭辞があり判別可能）。
struct ChunkWriter<'a> {
    shared: &'a Mutex<Box<dyn Write + Send>>,
    buf: Vec<u8>,
    threshold: usize,
}

/// チャンク書き出しのしきい値。これを超えたら完全な行までを共有 writer へ流す。
const CHUNK_BYTES: usize = 64 * 1024;

impl<'a> ChunkWriter<'a> {
    fn new(shared: &'a Mutex<Box<dyn Write + Send>>) -> Self {
        Self::with_threshold(shared, CHUNK_BYTES)
    }

    fn with_threshold(shared: &'a Mutex<Box<dyn Write + Send>>, threshold: usize) -> Self {
        Self {
            shared,
            buf: Vec::new(),
            threshold,
        }
    }

    /// バッファ内の最後の改行までを共有 writer に書き出す（行の途中は残す）。
    fn flush_complete_lines(&mut self) -> io::Result<()> {
        if let Some(pos) = self.buf.iter().rposition(|&b| b == b'\n') {
            let mut w = self.shared.lock().expect("writer mutex poisoned");
            w.write_all(&self.buf[..=pos])?;
            drop(w);
            self.buf.drain(..=pos);
        }
        Ok(())
    }
}

impl Write for ChunkWriter<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= self.threshold {
            self.flush_complete_lines()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // 残り（末尾の改行以降を含む）をすべて書き出す。
        if !self.buf.is_empty() {
            let mut w = self.shared.lock().expect("writer mutex poisoned");
            w.write_all(&self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }
}

/// 書き込みエラーを [`Stop`] に変換する。BrokenPipe は静かに終了、その他は致命的。
fn write_stop(e: io::Error) -> Stop {
    if e.kind() == ErrorKind::BrokenPipe {
        Stop::BrokenPipe
    } else {
        Stop::Fatal(anyhow::Error::new(e).context("failed to write output"))
    }
}

/// 1 ファイルを [`ChunkWriter`] 経由で `writer` へストリーム検索・書き込みする。
fn search_one(
    path: &Path,
    is_root: bool,
    re: &Regex,
    max_columns: usize,
    printer: &Printer,
    writer: &Mutex<Box<dyn Write + Send>>,
) -> Result<(), Stop> {
    let mut cw = ChunkWriter::new(writer);
    let search_result = search_file(path, re, max_columns, printer, &mut cw);

    match search_result {
        // チャンクフラッシュ中に書き込みが壊れた場合は出力経路が死んでいるので、
        // これ以上 flush せずに扱う。
        Err(SearchError::Write(e)) => Err(write_stop(e)),
        // Ok でも Read エラーでも、バッファ済みの一致行は必ず出力する
        // （途中まで書けていた結果を並列経路だけ捨てないため）。
        other => {
            if let Err(e) = cw.flush() {
                return Err(write_stop(e));
            }
            match other {
                Ok(()) => Ok(()),
                // 要求されたルートのファイル自体が読めない場合は fatal。
                Err(SearchError::Read(e)) if is_root => {
                    Err(Stop::Fatal(e.context("failed to read requested path")))
                }
                Err(SearchError::Read(e)) => {
                    eprintln!("warning: {e:#}");
                    Ok(())
                }
                Err(SearchError::Write(_)) => unreachable!("handled above"),
            }
        }
    }
}

/// 並列検索（`-j` が 2 以上）。候補ファイルを `par_bridge` でストリーム配分し、
/// 各ファイルを [`ChunkWriter`] 経由で `writer` へ書き込む。順序は非決定的。
///
/// 全候補を溜めずに走査と並列検索を重ねるため、巨大ツリーでも初回出力が早く、
/// `... | head` での早期終了（BrokenPipe）も効く。
fn search_parallel(
    root: &Path,
    re: &Regex,
    max_columns: usize,
    max_filesize: u64,
    printer: &Printer,
    writer: &Mutex<Box<dyn Write + Send>>,
) -> Result<(), Stop> {
    Walk::new(root).par_bridge().try_for_each(|result| {
        let entry = match result {
            Ok(entry) => entry,
            // 走査途中のエントリエラーは警告して継続（ルートの可読性は呼び出し前に検証済み）。
            Err(e) => {
                eprintln!("warning: {e}");
                return Ok(());
            }
        };
        let is_root = entry.depth() == 0;
        if !should_search(&entry, max_filesize) {
            return Ok(());
        }
        search_one(entry.path(), is_root, re, max_columns, printer, writer)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// 書いた内容を後から検査できるテスト用の共有 writer。
    #[derive(Clone)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn chunk_writer_emits_only_whole_lines_until_flush() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let shared: Mutex<Box<dyn Write + Send>> = Mutex::new(Box::new(Sink(sink.clone())));
        let mut cw = ChunkWriter::with_threshold(&shared, 8);

        // しきい値未満なので、まだ何も書き出されない。
        cw.write_all(b"aaaa\n").unwrap();
        assert!(sink.lock().unwrap().is_empty());

        // しきい値を超えると、最後の改行までがまとめて書き出される。
        cw.write_all(b"bbbb\n").unwrap();
        assert_eq!(&*sink.lock().unwrap(), b"aaaa\nbbbb\n");

        // 末尾の半端な行はバッファに残り、flush で初めて出る。
        cw.write_all(b"ccc").unwrap();
        assert_eq!(&*sink.lock().unwrap(), b"aaaa\nbbbb\n");
        cw.flush().unwrap();
        assert_eq!(&*sink.lock().unwrap(), b"aaaa\nbbbb\nccc");
    }

    #[test]
    fn chunk_writer_never_splits_a_line() {
        // 改行を含まないまましきい値を超えても、行の途中では書き出さない。
        let sink = Arc::new(Mutex::new(Vec::new()));
        let shared: Mutex<Box<dyn Write + Send>> = Mutex::new(Box::new(Sink(sink.clone())));
        let mut cw = ChunkWriter::with_threshold(&shared, 4);

        cw.write_all(b"abcdefgh").unwrap(); // 改行なし。閾値超でも出さない。
        assert!(sink.lock().unwrap().is_empty());
        cw.write_all(b"\n").unwrap(); // 改行が来たら一括で出る。
        assert_eq!(&*sink.lock().unwrap(), b"abcdefgh\n");
    }
}
