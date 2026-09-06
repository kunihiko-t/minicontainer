# MiniContainer

MiniContainerは、miniOSをゲストカーネルとして使い、RISC-V 64アプリケーションをQEMU仮想マシンで実行する小さなコンテナランタイムである。
仕組みを追える学習用実装を軸に、信頼できるコードを使った個人開発、デモ、OS教材、ランタイム実験で実際に動かせることを目標にしている。

## 対応環境

主要な開発環境はApple Silicon搭載macOSである。
継続検証の対象はUbuntu 24.04である。
Windowsは対象外である。

必要なツールはRust 1.98.0、`riscv64gc-unknown-none-elf`ターゲット、QEMU 8.2.0以上、Gitである。
`xtask setup`自体はツールの導入や更新を行わず、環境を診断する。
初回は起動元のCargoやrustupによる依存取得やツールチェーンの導入が発生することがある。

## クイックスタート

RustとGit、QEMUを用意し、このリポジトリを取得する。
ツールの導入と診断は[開発環境](docs/guide/02-dev-environment-xtask.md)を参照する。

```sh
git clone https://github.com/kunihiko-t/minicontainer.git
cd minicontainer
```

`minictr run hello`で、同じ結果を再現できるhelloゲストを実行する。
標準出力に`hello stdout`、標準エラー出力に`hello stderr`が届き、終了コード42で終わる。
以下は同じシェルで、MiniContainerのリポジトリ直下から順に実行する。
作業用のminiOSとストアは一時ディレクトリーに作成するため、長期保存には別の保存先を指定する。

```sh
MINIOS="$(mktemp -d)/minios"
STORE="$(mktemp -d)/store"
```

まず開発用バイナリーをビルドする。

```sh
cargo xtask setup
cargo build -p minictr --locked
```

ゲストカーネルはminiOSの固定リビジョンからビルドする。
`$MINIOS`はminiOSのチェックアウト先であり、この時点では存在しないパスである。

```sh
git clone https://github.com/kunihiko-t/minios.git "$MINIOS"
git -C "$MINIOS" checkout 9be99255a59d58d19db25b835af0e28a8d2a4036
cargo build --manifest-path "$MINIOS/Cargo.toml" --target-dir "$MINIOS/target" -p minios-kernel --bin minios-kernel --target riscv64gc-unknown-none-elf --locked
```

ビルド成果物`$MINIOS/target/riscv64gc-unknown-none-elf/debug/minios-kernel`が実行カーネルである。
Guest ABIは`minios-abi-v0.1.1`に固定している。

次にhello bundleを`store`へ取り込み、`hello`タグを作る。
`$STORE`は絶対パスで指定した保存先であり、`Store::new`が作成する。

```sh
cargo run -p xtask --locked --example prepare_hello_store -- "$STORE"
```

最後に`minictr run`で実行する。

```sh
exit_code=0
cargo run -p minictr --locked -- run --store "$STORE" --kernel "$MINIOS/target/riscv64gc-unknown-none-elf/debug/minios-kernel" hello || exit_code=$?
echo "exit=$exit_code"
```

期待する結果は次の通りである。

```text
hello stdout
exit=42
```

`hello stderr`は標準エラー出力に届く。Cargoのビルド状況も標準エラー出力に表示される。
この例の42は意図した終了コードであり、シェルの`set -e`が有効でも結果を確認できる形にしている。
`minictr`は0〜255のゲスト終了コードをそのまま返し、範囲外はホスト側の失敗として扱う。
使い方の誤りは終了コード2、ホスト側の失敗（`store`の解決失敗、QEMUの失敗、タイムアウトを含む）は終了コード125になる。

## 現在の機能と制限

`minictr`が実装するコマンドは`run`、`help`、`--version`である。

```text
usage: minictr run [--store PATH] [--kernel PATH] [--timeout-ms N] IMAGE
```

`help`と`--help`は上記のusageを標準出力へ出して0で終わる。
`--version`は`minictr 0.1.0`を標準出力へ出して0で終わる。

`--store`と`--kernel`を省略した値は環境変数`MINICTR_STORE`、`MINICTR_KERNEL`、なければ`$HOME/.minicontainer`以下から解決する。
`--timeout-ms`の既定値は5000である。

MiniBundleの構築と検証、SHA-256ダイジェスト、manifestの制限、content-addressed storeへの取り込み、タグ付け、解決ができる。
UART control frame decoderは分割入力を復元し、不正なheaderと64 KiBを超えるpayloadを拒否する。
QEMUバックエンドは固定した引数で起動し、通常の成功経路とエラー経路で子プロセスの回収と一時領域の削除を試みる。
後始末の失敗もエラーとして返す。ホストの停止や`SIGKILL`による強制終了では、後始末を実行できない場合がある。
ゲスト出力はメモリーに蓄積し、実行完了後に表示する。stdout、stderr、診断の合計は1 MiBが上限であり、超過はホスト側の失敗として扱う。対話入力とリアルタイムの出力表示には対応していない。

OCI互換、ネットワーク、永続ボリューム、Linuxアプリケーション互換、マルチテナント分離は現在の機能ではない。
詳細は[脅威モデル](docs/reference/threat-model.md)で確認できる。

## 検証

公開前検証の入口は次の二つである。

```sh
cargo xtask setup
cargo xtask check
```

`check`はrustfmt、Markdownリンク、公開対象ファイル、crateごとのClippyと単体テスト、lockfileを使ったworkspaceのビルド、実QEMU end-to-end検証を15段階で実行する。
E2Eは固定リビジョンのminiOSカーネルをビルドし、`minictr run hello`の標準出力、標準エラー出力、終了コード42、QEMUの回収、一時ディレクトリーの後始末を確認する。
タイムアウト、不正フレーム、出力上限の失敗経路では、非0終了と残留物がないことも確認する。

## 教材

設計の全体像は[アーキテクチャ](docs/design/architecture.md)に記載している。
学習の入口は[ガイド索引](docs/guide/README.md)である。
ゲストとの境界と制約は[脅威モデル](docs/reference/threat-model.md)で確認できる。
実装済みの節目と次の実装順は[ロードマップ](docs/reference/roadmap.md)に記載している。

## 制約とセキュリティー

MiniContainerは本番用のセキュリティー境界ではありません。
未信頼コードを扱うマルチテナント環境には使用しない。
脆弱性の報告方法は[Security Policy](SECURITY.md)を参照する。

## ライセンス

MiniContainerはMIT LicenseまたはApache License 2.0の条件で利用できる。
詳細は[LICENSE-MIT](LICENSE-MIT)と[LICENSE-APACHE](LICENSE-APACHE)を参照する。
SPDX表記は`MIT OR Apache-2.0`である。
