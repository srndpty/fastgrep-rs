//! CLI 全体の統合テスト（assert_cmd でビルド済みバイナリを実行する）。
//!
//! 出力フォーマット・終了コード・各オプション・並列/逐次の一致などを検証する。
//! 色に依存しないテストは `--color never` を明示してプラットフォーム差を避ける。

use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// テスト対象バイナリの新しいコマンドを作る。
fn cmd() -> Command {
    Command::cargo_bin("fastgrep-rs").unwrap()
}

#[test]
fn finds_and_formats_matches() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.txt"), "alpha\nTODO here\nomega\n").unwrap();

    cmd()
        .args(["TODO", "--color", "never"])
        .arg(dir.path())
        .assert()
        .success()
        // path:line_number:line 形式。パス末尾だけ確認する。
        .stdout(predicate::str::contains("a.txt:2:TODO here"));
}

#[test]
fn no_match_is_success_with_empty_stdout() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.txt"), "alpha\nbeta\n").unwrap();

    cmd()
        .args(["ZZZ", "--color", "never"])
        .arg(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn ignore_case_flag() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.txt"), "Hello TODO\n").unwrap();

    // -i ありで一致。
    cmd()
        .args(["todo", "-i", "--color", "never"])
        .arg(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("a.txt:1:"));

    // -i なしでは一致しない。
    cmd()
        .args(["todo", "--color", "never"])
        .arg(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn single_file_path() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("only.txt");
    fs::write(&file, "TODO one\nno\nTODO two\n").unwrap();

    cmd()
        .args(["TODO", "--color", "never"])
        .arg(&file)
        .assert()
        .success()
        .stdout(predicate::str::contains("only.txt:1:TODO one"))
        .stdout(predicate::str::contains("only.txt:3:TODO two"));
}

#[test]
fn nonexistent_path_fails() {
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("does-not-exist");

    cmd()
        .arg("x")
        .arg(&missing)
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot access path"));
}

#[test]
fn invalid_regex_fails() {
    let dir = TempDir::new().unwrap();

    cmd()
        .arg("(") // 閉じられていないグループ
        .arg(dir.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid regex pattern"));
}

#[test]
fn skips_files_over_max_filesize() {
    let dir = TempDir::new().unwrap();
    // 既定の上限 1M を超える大きいファイル（2 行目に一致を置く）。
    let big = format!("{}\nTODO big\n", "x".repeat(2 * 1024 * 1024));
    fs::write(dir.path().join("big.txt"), big).unwrap();
    fs::write(dir.path().join("small.txt"), "TODO small\n").unwrap();

    // 既定では big.txt はサイズ超過でスキップ。
    cmd()
        .args(["TODO", "--color", "never"])
        .arg(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("small.txt:1:TODO small"))
        .stdout(predicate::str::contains("big.txt").not());

    // --max-filesize 0 なら big.txt も検索対象（1 行目は超長行なのでスキップ、2 行目が一致）。
    cmd()
        .args(["TODO", "--max-filesize", "0", "--color", "never"])
        .arg(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("big.txt:2:TODO big"));
}

#[test]
fn binary_file_is_skipped() {
    let dir = TempDir::new().unwrap();
    // 先頭に NUL を含むファイルはバイナリ扱いでスキップ。
    let mut data = vec![0u8];
    data.extend_from_slice(b"TODO match\n");
    fs::write(dir.path().join("bin.dat"), data).unwrap();
    fs::write(dir.path().join("text.txt"), "TODO text\n").unwrap();

    cmd()
        .args(["TODO", "--color", "never"])
        .arg(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("text.txt:1:TODO text"))
        .stdout(predicate::str::contains("bin.dat").not());
}

#[test]
fn max_columns_truncates_body() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("a.txt"),
        format!("TODO {}\n", "y".repeat(100)),
    )
    .unwrap();

    cmd()
        .args(["TODO", "--max-columns", "8", "--color", "never"])
        .arg(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("…[+").and(predicate::str::contains("chars]")));
}

#[test]
fn color_never_is_plain_and_always_has_ansi() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.txt"), "TODO x\n").unwrap();

    cmd()
        .args(["TODO", "--color", "never"])
        .arg(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{1b}[").not());

    cmd()
        .args(["TODO", "--color", "always"])
        .arg(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{1b}["));
}

#[test]
fn sequential_and_parallel_produce_same_matches() {
    let dir = TempDir::new().unwrap();
    for i in 0..20 {
        let content = (0..5)
            .map(|j| format!("f{i} TODO {j}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(dir.path().join(format!("f{i}.txt")), content).unwrap();
    }

    // 並列は順序が非決定的なのでソートして集合として比較する。
    let run = |jobs: &str| -> Vec<String> {
        let output = cmd()
            .args(["TODO", "-j", jobs, "--color", "never"])
            .arg(dir.path())
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let mut lines: Vec<String> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        lines.sort();
        lines
    };

    let seq = run("1");
    assert_eq!(seq.len(), 100, "20 ファイル × 5 行 = 100 一致のはず");
    assert_eq!(seq, run("4"), "逐次と並列で一致集合が同じはず");
}
