# 複数taskとfault分離のサンプル

同梱の`guest-hello`とMiniOSの既存検査guest `minios-guest-proc-fault`を使い、
公開CLIで複数imageを構築して実行する。新しいguestやABIは追加しない。
正常2taskの共有stdout/stderrとPID別結果を確認し、続いて不正命令を起こすtaskとsurvivorを一緒に実行する。

## 前提とkernelの違い

Rust 1.98.0、`riscv64gc-unknown-none-elf`、QEMU 8.2.0以上、Gitを用意する。
以下はMiniContainerのリポジトリ直下から同じシェルで順に実行する。
`build-multi`はv1.1.0から配布CLIでも使える。v1.0.0の配布CLIではこのcommandを使えない。
以下はsourceからビルドする手順である。

このサンプルはtask-local fault処理を含むMiniOS revision
`086f2e3fa54f751cd28c5fcd2bed205830bd2934`を明示してビルドする。
このSHAはv1.1.0の標準配布kernelと同じである。利用者の既存kernelファイルを自動更新するものではない。

| kernel | 正常2task | fault＋survivor |
| --- | --- | --- |
| v1.0.0のkernel `4865f9be97a6cdcd77c71e36b1ba426b49bd73d7`（現行CLIで実行） | PID 0/1とも42、CLI終了42 | guest trapでVM全体が失敗し、host error 125 |
| v1.1.0のkernel `086f2e3fa54f751cd28c5fcd2bed205830bd2934` | PID 0/1とも42、CLI終了42 | PID 0は70、PID 1は42、CLI終了70 |

この結果はUbuntuのQEMUで確認したものであり、物理FPGAやmacOS実機の動作を示すものではない。
正常guestの42は意図した終了codeである。以下では`set -e`が有効でも結果を確認できるように終了codeを保存する。

## CLI、guest、候補kernelをビルドする

```sh
cargo xtask setup
cargo build -p minictr --locked
cargo build --manifest-path examples/guest-hello/Cargo.toml --target riscv64gc-unknown-none-elf --release --locked --config 'target.riscv64gc-unknown-none-elf.rustflags=["-C", "link-arg=-Tlinker.ld"]' --config 'build.target-dir="target/guest-hello"'

WORK="$(mktemp -d)"
MINIOS="$WORK/minios"
STORE="$WORK/store"
CANDIDATE_REV=086f2e3fa54f751cd28c5fcd2bed205830bd2934
git clone https://github.com/kunihiko-t/minios.git "$MINIOS"
git -C "$MINIOS" checkout --detach "$CANDIDATE_REV"
cargo build --manifest-path "$MINIOS/Cargo.toml" --target-dir "$WORK/target" -p minios-kernel --bin minios-kernel -p minios-guest --bin minios-guest-proc-fault --target riscv64gc-unknown-none-elf --locked

CLI=./target/debug/minictr
HELLO=target/guest-hello/riscv64gc-unknown-none-elf/release/guest-hello
FAULT="$WORK/target/riscv64gc-unknown-none-elf/debug/minios-guest-proc-fault"
KERNEL="$WORK/target/riscv64gc-unknown-none-elf/debug/minios-kernel"
```

MiniOSの検査sourceは指定したSHAに固定し、作業用checkout、target、storeを一時directoryに分離する。

## 正常2taskと共有I/O

同じELFを異なるimage名で登録する。入力順が初期PID順になる。

```sh
"$CLI" image build-multi normal --store "$STORE" --image first "$HELLO" --image second "$HELLO"
status=0
"$CLI" run --store "$STORE" --kernel "$KERNEL" --timeout-ms 10000 normal > "$WORK/normal.out" 2> "$WORK/normal.err" || status=$?
cat "$WORK/normal.out"
cat "$WORK/normal.err"
echo "exit=$status"
test "$status" -eq 42
```

stdoutには`hello from guest`が2行、stderrには`guest stderr`が2行と次のPID結果が届く。

```text
minictr: process pid=0 code=42
minictr: process pid=1 code=42
```

stdout/stderrは全taskの共有streamで、task別の出力ファイルにはならない。
guestの出力とPID結果の到着順は任意であり、stdout/stderrを別ファイルに保存した場合も両streamの間の順序は復元できない。
PID結果はstderrの診断表示で、文言自体は安定した公開契約ではない。

## 不正命令taskとsurvivor

`--arg illegal`は直前のfault imageへ渡る。PID 0が例外で終了しても、PID 1は最後まで実行する。

```sh
"$CLI" image build-multi fault --store "$STORE" --image fault "$FAULT" --arg illegal --image survivor "$HELLO"
status=0
"$CLI" run --store "$STORE" --kernel "$KERNEL" --timeout-ms 10000 fault > "$WORK/fault.out" 2> "$WORK/fault.err" || status=$?
cat "$WORK/fault.out"
cat "$WORK/fault.err"
echo "exit=$status"
test "$status" -eq 70
```

stdoutにはsurvivorの`hello from guest`が1行、stderrには`guest stderr`が1行と次の結果が届く。

```text
minictr: process pid=0 code=70
minictr: process pid=1 code=42
```

CLI全体の終了codeは、PID順で最初の非zero codeである。ここではPID 0の70になる。
foregroundでのみ実行でき、manifest v2の`--detach`は起動前に拒否される。
より詳しい契約は[複数task実行](../../docs/reference/multi-task.md)を参照する。

## 回収と既存v1経路

foregroundの終了時にQEMU、instance state、runtime用payloadは回収される。
storeのimageとこのサンプルのビルド成果物・出力ログは残る。

```sh
"$CLI" ps --store "$STORE"
```

ヘッダーだけでinstance行がないことを確認する。
既存`image build`は同じguestからv1 bundleを構築する。

```sh
"$CLI" image build --store "$STORE" hello "$HELLO"
status=0
"$CLI" run --store "$STORE" --kernel "$KERNEL" hello || status=$?
echo "exit=$status"
test "$status" -eq 42
```

元のリリース固定kernelを使うv1手順は[クイックスタート](../../README.md#クイックスタート)で確認できる。
ログを見終えたら作業用directoryを消す。

```sh
rm -rf "$WORK"
```
