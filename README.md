# MiniContainer

MiniContainerは、miniOSをゲストカーネルとして使い、一つのRISC-V 64アプリケーションを一つのQEMU仮想マシンで実行する学習用マイクロVMランタイムである。
仕組みを追える実装を軸に、信頼できる静的RISC-V 64ゲストをローカルで動かす個人開発、デモ、OS教材、ランタイム実験に使う。
DockerやOCI、Linuxコンテナとの互換性はない。

## 対応環境

主要な開発環境はApple Silicon搭載macOSである。
継続検証はUbuntu 24.04とApple Silicon搭載macOSである。
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

同梱の最小ゲスト例をビルドし、`image build`で登録して`minictr run`で実行する。
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

次に同梱ゲスト例を静的RISC-V 64 ELFへビルドする。

```sh
cargo build --manifest-path examples/guest-hello/Cargo.toml --target riscv64gc-unknown-none-elf --release --locked --config 'target.riscv64gc-unknown-none-elf.rustflags=["-C", "link-arg=-Tlinker.ld"]' --config 'build.target-dir="target/guest-hello"'
GUEST="target/guest-hello/riscv64gc-unknown-none-elf/release/guest-hello"
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

次にゲストELFを`image build`で`store`へ登録し、`hello`タグを付ける。
`$STORE`は絶対パスで指定した保存先であり、`Store::new`が作成する。
8 MiBはbundle全体の上限であり、ELF単体で超える入力は本体を読む前に拒否する。
実際に収まる最大のELFはheader・manifest・padding分だけ小さい。
ELFの中身はhostでは検証せずguestのloaderが検証する。

```sh
cargo run -p minictr --locked -- image build --store "$STORE" hello "$GUEST"
```

成功すると、タグとSHA-256 digestの一行だけが表示される。

```text
hello sha256:<64桁の小文字16進数>
```

登録内容は`image inspect`で確認する。

```sh
cargo run -p minictr --locked -- image inspect --store "$STORE" hello
```

```text
tag: hello
name: hello
digest: sha256:<buildと同じ64桁の小文字16進数>
args: 0
elf-bytes: <ゲストELFのbyte数>
```

実行前に`doctor`で環境を確認する。`run`と同じ`--store`と`--kernel`を渡す。

```sh
cargo run -p minictr --locked -- doctor --store "$STORE" --kernel "$MINIOS/target/riscv64gc-unknown-none-elf/debug/minios-kernel"
```

```text
qemu: ok qemu-system-riscv64 <8.2.0以上>
kernel: ok <kernelのpath> (<byte数> bytes)
store: ok <storeのpath>
summary: passed 3/3 checks
```

全検査の成功で終了コード0、一つでも失敗したら`fail`行をすべて出して終了コード1になる。
診断は環境を変更しない。失敗行の`fix:`に従って直してから実行する。

登録したimageは`image export`でファイルへ取り出す。タグの代わりに`sha256:`付きdigestでも指定できる。

```sh
cargo run -p minictr --locked -- image export --store "$STORE" hello --output ./hello.mcb
```

`hello sha256:<64桁>`と表示され、`./hello.mcb`にbundleバイト列がそのまま書き出される。
出力先にファイルがある場合は上書きせず失敗する。

最後に`minictr run`で実行する。

```sh
exit_code=0
cargo run -p minictr --locked -- run --store "$STORE" --kernel "$MINIOS/target/riscv64gc-unknown-none-elf/debug/minios-kernel" hello || exit_code=$?
echo "exit=$exit_code"
```

期待する結果は、ゲストの標準出力がそのまま表示され、`exit=`にゲストの終了コードが入ることである。
同梱ゲストは標準出力へ`hello from guest`、標準エラー出力へ`guest stderr`と書いて42で終わる。

```text
hello from guest
exit=42
```

guest resourceを変える場合は`--memory`と`--cpus`を付ける。どちらも省略時は128 MiBと1 vCPUのままである。

```sh
cargo run -p minictr --locked -- run --store "$STORE" --kernel "$MINIOS/target/riscv64gc-unknown-none-elf/debug/minios-kernel" --memory 256 --cpus 2 hello
```

ゲストの標準エラー出力は`minictr`の標準エラー出力に届く。Cargoのビルド状況も標準エラー出力に表示される。
この例の42は意図した終了コードであり、シェルの`set -e`が有効でも結果を確認できる形にしている。
`minictr`は0〜255のゲスト終了コードをそのまま返し、範囲外はホスト側の失敗として扱う。
使い方の誤りは終了コード2、ホスト側の失敗（`image build`の失敗、`store`の解決失敗、QEMUの失敗、タイムアウトを含む）は終了コード125になる。
実行中のCtrl-CはQEMUの終了をホスト側の失敗として回収し、後始末を経て終了コード125で終わる。

## 現在の機能と制限

`minictr`が実装するコマンドは`run`、`doctor`、`image build`、`image import`、`image export`、`image list`、`image inspect`、`image remove`、`image prune`、`image export-oci`、`image import-oci`、`image pull-oci`、`help`、`--version`である。
構文と解決、登録と確認の詳細は[第9章](docs/guide/09-minictr-run.md)を参照する。

```text
usage: minictr run [--store PATH] [--kernel PATH] [--timeout-ms N] [--memory MIB] [--cpus N] IMAGE
usage: minictr doctor [--store PATH] [--kernel PATH]
usage: minictr image build [--store PATH] [--arg VALUE]... IMAGE ELF
usage: minictr image import [--store PATH] IMAGE FILE
usage: minictr image export [--store PATH] IMAGE --output PATH
usage: minictr image list [--store PATH]
usage: minictr image inspect [--store PATH] IMAGE
usage: minictr image remove [--store PATH] IMAGE
usage: minictr image prune [--store PATH] [--dry-run] [--force]
usage: minictr image export-oci [--store PATH] IMAGE --output DIR
usage: minictr image import-oci [--store PATH] IMAGE DIR
usage: minictr image pull-oci [--store PATH] IMAGE REFERENCE
```

`help`と`--help`は上記のusageを標準出力へ出して0で終わる。
`--version`は`minictr 0.1.0`を標準出力へ出して0で終わる。

`image build`は静的RISC-V 64 ELFからMiniBundleを構築し、指定したタグでローカルストアへ登録する。
成功すると`IMAGE sha256:<digest>`の一行だけを標準出力へ出す。
`image import`は配布されたMiniBundleファイルを検証してから同じ形で登録し、manifestの名前は書き換えない。
`image export`はタグまたは`sha256:`付きdigestで解決したbundleを検証して`--output`へ書き出す。出力先の上書きはしない。
`image list`はタグの一覧、`image inspect`は`tag`、`name`、`digest`、`args`、`elf-bytes`の5行を出す。
`doctor`はQEMUのversion、kernel file、store rootを診断し、各検査の`ok`または`fail`と`summary`の4行を標準出力へ出す。全検査の成功で終了コード0、一つでも失敗したら終了コード1になり、診断自体は環境を変更しない。
`image remove`は指定タグだけを削除して`IMAGE removed`と出し、blobと他のタグは保持する。存在しないタグは型付きエラーで失敗する。
`image prune`は未参照blobの検出だけが既定動作であり、`--force`の指定時だけ削除する。削除前に参照を再確認し、部分失敗は個別に報告する。

`image export-oci`はstoreのimageをOCI Image Layoutのdirectoryへ書き出し、`image import-oci`はlayoutを検証してstoreへ登録する。
`image pull-oci`はregistryからdigest pinで匿名pullし、指定したタグで登録する。
成功すると`IMAGE sha256:<digest>`の一行だけを標準出力へ出す。

```sh
cargo run -p minictr --locked -- image pull-oci --store "$STORE" hello registry.example.com:5000/demo/hello@sha256:<64桁の小文字16進数>
```

pullはHTTPSだけを使い、認証はしない。参照はtagではなくdigest pinだけを受け入れる。
redirectは5回まで、接続10秒・要求60秒のtimeout、manifestとconfigは64 KiB・layerは8 MiBの上限で取得する。
詳細は[第12章](docs/guide/12-oci-image-spec.md)を参照する。

`--store`と`--kernel`を省略した値は環境変数`MINICTR_STORE`、`MINICTR_KERNEL`、なければ`$HOME/.minicontainer`以下から解決する。
`--timeout-ms`の既定値は5000である。

MiniBundleの構築と検証、SHA-256ダイジェスト、manifestの制限、content-addressed storeへの取り込み、タグ付け、解決、一覧ができる。
UART control frame decoderは分割入力を復元し、不正なheaderと64 KiBを超えるpayloadを拒否する。
QEMUバックエンドは固定した引数で起動し、通常の成功経路とエラー経路で子プロセスの回収と一時領域の削除を試みる。
後始末の失敗もエラーとして返す。ホストの停止や`SIGKILL`、`SIGTERM`による強制終了では、後始末を実行できない場合がある。
ゲスト出力はdecodeされ次第、標準出力と標準エラー出力へ区別して逐次表示する。stdout、stderr、診断の合計は表示済みも含めて1 MiBが上限であり、超過はホスト側の失敗として扱う。対話入力には対応していない。

OCI互換はMiniBundle用artifactの配布形式（export、import、匿名pull）だけであり、Docker runtime互換とLinuxアプリケーション実行互換ではない。
ネットワークはregistry pullのHTTPS clientだけであり、ゲストへの提供、永続ボリューム、マルチテナント分離は現在の機能ではない。
詳細は[脅威モデル](docs/reference/threat-model.md)で確認できる。

## 検証

公開前検証の入口は次の三つである。

```sh
cargo xtask setup
cargo xtask check-host
cargo xtask check
```

`check`はrustfmt、Markdownリンク、公開対象ファイル、crateごとのClippyと単体テスト、同梱ゲストのビルド、lockfileを使ったworkspaceのビルド、実QEMU end-to-end検証を16段階で実行する。
`check-host`は実QEMU検証を除く15段階を実行する。
CIはUbuntu 24.04で`setup`と`check`、Apple SiliconのmacOSで`check-host`を実行する。
E2Eは固定リビジョンのminiOSカーネルをビルドし、同梱ゲストを公開CLIの`image build`、`image inspect`、`run`へ一続きで通して、標準出力、標準エラー出力、終了コード42、QEMUの回収、一時ディレクトリーの後始末を確認する。
タイムアウト、不正フレーム、出力上限、割り込みの失敗経路では、非0終了と残留物がないことも確認する。

## 配布物

`v0.1.0`タグのpushで、`minicontainer-0.1.0-aarch64-apple-darwin.tar.gz`と`minicontainer-0.1.0-x86_64-unknown-linux-gnu.tar.gz`を`cargo xtask dist`で構築する。
archiveは`minictr`、固定revisionのminiOS kernel、ライセンス、`MANIFEST.txt`、`SHA256SUMS`を含む。
展開後は`minictr run --kernel kernel/minios.bin`の形で実行する。
詳細は[v0.1.0配布手順](docs/reference/releasing.md)を参照する。

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
