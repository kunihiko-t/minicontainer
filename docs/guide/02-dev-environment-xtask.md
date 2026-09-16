# 開発環境と`cargo xtask`

この章は、Rust、RISC-V target、QEMU、Gitを診断し、同じrelease gateをlocalとCIで実行できるようにする。
gateの段階の内訳は第11章を参照する。

## 対象環境

主要な開発環境はApple Silicon搭載macOS、継続検証はUbuntu 24.04とApple Silicon搭載macOSである。
Windowsは対象外であり、Windows上の動作は検証しない。

必要なtoolはRust 1.98.0、`riscv64gc-unknown-none-elf` target、QEMU 8.2.0以上、Gitである。
Rustは1.98.0 stableの完全一致を要求する。

## setupで診断する

最初に`cargo xtask setup`を実行する。
`xtask setup`自体はツールの導入や更新を行わず、有無とバージョンを診断する。
ただし起動元のCargoやrustupは、初回の依存取得や指定ツールチェーンの導入を行うことがある。
不足があれば導入手順つきの診断を出して失敗するため、表示に従ってtoolを揃える。

## checkで検証する

次に`cargo xtask check`を実行する。
書式、文書link、公開条件、crateごとのClippyと単体試験、固定seedのbounded fuzz smoke、同梱ゲストのbuild、workspace build、実QEMU end-to-end検証を18段階で順に実行し、最初の失敗で停止する。
QEMUを使わない確認には、最終段階を除く17段階の`cargo xtask check-host`を使う。
依存解決を伴うphaseはすべて`--locked`で実行する。

CIはUbuntu 24.04で`setup`と`check`、Apple SiliconのmacOSで`check-host`を実行する。
変更後は手元でgateを通してから共有する。

## 初回実行までの流れ

RustとCargoを使える状態にするにはrustupを用意し、リポジトリの`rust-toolchain.toml`が指定するツールチェーンを導入する。
QEMUはmacOSでは`brew install qemu`、Ubuntuでは`sudo apt-get install qemu-system-misc`で導入する。
Gitもあらかじめ用意する。

`minictr run hello`までの手順は[READMEのクイックスタート](../../README.md#クイックスタート)に従う。
同梱ゲスト例をbuildし、固定revisionのminiOSからguest kernelをbuildし、公開CLIで登録、検査、実行する。
storeとkernelの解決、実行結果の読み方は第9章、失敗の切り分けは第10章を参照する。
