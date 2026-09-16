# MiniContainer

MiniContainerは、miniOSをゲストカーネルとして使い、一つのRISC-V 64アプリケーションを一つのQEMU仮想マシンで実行する学習用マイクロVMランタイムである。
仕組みを追える実装を軸に、信頼できる静的RISC-V 64ゲストをローカルで動かす個人開発、デモ、OS教材、ランタイム実験に使う。

`minictr` binary一つで、静的ELFをMiniBundleとして登録し、QEMU上のminiOSで実行し、終了コードと標準入出力をホストへ透過する。
imageはcontent-addressed storeに置き、ファイルとOCI Image Layoutの両方で持ち出しと取り込みができる。
v1.0.0で公開契約（MiniBundle format、Guest ABI、CLI構文と終了コード、instance state、OCI対応）を固定した。
変更は[v1.0.0 release notes](docs/reference/v1.0.0-release-notes.md)に、保証と検証根拠の対応は[公開契約の監査表](docs/reference/public-contract.md)に記載している。

DockerやOCI runtime、Linuxコンテナとの互換性はない。
OCI対応はMiniBundle用artifactの配布形式に限る。
MiniContainerは本番用のセキュリティー境界ではありません。

## 対応環境

主要な開発環境はApple Silicon搭載macOSである。
継続検証はUbuntu 24.04とApple Silicon搭載macOSである。
Windowsは対象外である。

必要なツールはRust 1.98.0、`riscv64gc-unknown-none-elf`ターゲット、QEMU 8.2.0以上、Gitである。
`cargo xtask setup`はツールの導入や更新を行わず、環境を診断する。
ツールの導入手順は[開発環境](docs/guide/02-dev-environment-xtask.md)を参照する。

## 配布物から始める

`v*`タグごとに、macOS arm64用とLinux x86_64用のarchiveをGitHub Releaseへ添付している。
archiveは`minictr`、固定revisionのminiOS kernel（`kernel/minios.bin`）、ライセンス、`MANIFEST.txt`、`SHA256SUMS`を含む。
導入前にchecksumとbuild provenanceを確認する。

```sh
sha256sum -c 'minicontainer-<version>-x86_64-unknown-linux-gnu.tar.gz.sha256'
gh attestation verify 'minicontainer-<version>-x86_64-unknown-linux-gnu.tar.gz' --owner kunihiko-t
tar xzf 'minicontainer-<version>-x86_64-unknown-linux-gnu.tar.gz'
cd 'minicontainer-<version>-x86_64-unknown-linux-gnu'
./minictr --version
```

展開後は`./minictr run --kernel kernel/minios.bin IMAGE`の形で実行する。
archiveにはゲスト例のsourceを含まないため、実行するELFは利用者が用意するか、次節の手順でこのリポジトリのゲスト例をビルドする。
checksumはファイルの完全性だけを示し、生成元の証明にはならない。
出所まで確認する場合は`gh attestation verify`を必ず通す。
詳細は[配布手順](docs/reference/releasing.md)を参照する。

## クイックスタート

sourceからビルドし、同梱の最小ゲスト例を登録して実行する。
以下は同じシェルで、MiniContainerのリポジトリ直下から順に実行する。
作業用のminiOSとストアは一時ディレクトリーに作成するため、長期保存には別の保存先を指定する。

```sh
git clone https://github.com/kunihiko-t/minicontainer.git
cd minicontainer
MINIOS="$(mktemp -d)/minios"
STORE="$(mktemp -d)/store"
KERNEL="$MINIOS/target/riscv64gc-unknown-none-elf/debug/minios-kernel"
```

### ホスト側と同梱ゲストのビルド

`cargo xtask setup`で環境を診断し、`minictr`をビルドする。

```sh
cargo xtask setup
cargo build -p minictr --locked
```

同梱ゲスト例`guest-hello`を静的RISC-V 64 ELFへビルドする。
`guest-hello`は標準出力へ`hello from guest`、標準エラー出力へ`guest stderr`と書いて42で終わる。

```sh
cargo build --manifest-path examples/guest-hello/Cargo.toml --target riscv64gc-unknown-none-elf --release --locked --config 'target.riscv64gc-unknown-none-elf.rustflags=["-C", "link-arg=-Tlinker.ld"]' --config 'build.target-dir="target/guest-hello"'
GUEST="target/guest-hello/riscv64gc-unknown-none-elf/release/guest-hello"
```

### ゲストカーネルのビルド

ゲストカーネルはminiOSの固定リビジョンからビルドする。
Guest ABIは`minios-abi-v0.2.0`に固定している。

```sh
git clone https://github.com/kunihiko-t/minios.git "$MINIOS"
git -C "$MINIOS" checkout 4865f9be97a6cdcd77c71e36b1ba426b49bd73d7
cargo build --manifest-path "$MINIOS/Cargo.toml" --target-dir "$MINIOS/target" -p minios-kernel --bin minios-kernel --target riscv64gc-unknown-none-elf --locked
```

ビルド成果物が`$KERNEL`である。

### imageの登録と確認

ゲストELFを`image build`でstoreへ登録し、`hello`タグを付ける。
bundle全体の上限は6 MiBであり、ELF単体で超える入力は本体を読む前に拒否する。
ELFの中身はホストでは検証せず、ゲストのloaderが検証する。

```sh
cargo run -p minictr --locked -- image build --store "$STORE" hello "$GUEST"
```

成功すると、タグとSHA-256 digestの一行だけが表示される。

```text
hello sha256:<64桁の小文字16進数>
```

登録内容は`image inspect`で、環境は`doctor`で確認する。

```sh
cargo run -p minictr --locked -- image inspect --store "$STORE" hello
cargo run -p minictr --locked -- doctor --store "$STORE" --kernel "$KERNEL"
```

```text
tag: hello
name: hello
digest: sha256:<buildと同じ64桁の小文字16進数>
args: 0
elf-bytes: <ゲストELFのbyte数>
qemu: ok qemu-system-riscv64 <8.2.0以上>
kernel: ok <kernelのpath> (<byte数> bytes)
store: ok <storeのpath>
summary: passed 3/3 checks
```

`doctor`は全検査の成功で終了コード0、一つでも失敗したら`fail`行をすべて出して終了コード1になる。
診断は環境を変更しないため、`fail`行の`fix:`に従って直してから再実行できる。

### 実行

`minictr run`で実行する。
ゲストの標準出力と標準エラー出力はそれぞれ`minictr`の標準出力と標準エラー出力へ届き、ゲストの終了コードがそのまま返る。

```sh
exit_code=0
cargo run -p minictr --locked -- run --store "$STORE" --kernel "$KERNEL" hello || exit_code=$?
echo "exit=$exit_code"
```

```text
hello from guest
exit=42
```

この例の42は意図した終了コードであり、シェルの`set -e`が有効でも結果を確認できる形にしている。
Cargoのビルド状況も標準エラー出力に表示される。

guest resourceを変える場合は`--memory`と`--cpus`を付ける。
省略時は128 MiBと1 vCPUである。

```sh
cargo run -p minictr --locked -- run --store "$STORE" --kernel "$KERNEL" --memory 256 --cpus 2 hello
```

### 標準入力の転送

標準入力が端末でなければ、その内容はゲストのstdinへ転送される。
同梱の`guest-echo`は入力をそのまま書き戻し、EOFの後に42で終わる。

```sh
cargo build --manifest-path examples/guest-echo/Cargo.toml --target riscv64gc-unknown-none-elf --release --locked --config 'target.riscv64gc-unknown-none-elf.rustflags=["-C", "link-arg=-Tlinker.ld"]' --config 'build.target-dir="target/guest-echo"'
cargo run -p minictr --locked -- image build --store "$STORE" echo target/guest-echo/riscv64gc-unknown-none-elf/release/guest-echo
printf 'ping' | cargo run -p minictr --locked -- run --store "$STORE" --kernel "$KERNEL" echo
```

端末からの起動ではゲストの`read`は直ちに0を返し、`--detach`では入力経路がない。

### detached実行と回収

`run --detach`はゲストの起動を確認してinstance idだけを出して終わり、QEMUをsupervisorなしで残す。
`ps`で一覧し、`stop`で回収する。

```sh
id="$(cargo run -p minictr --locked -- run --detach --store "$STORE" --kernel "$KERNEL" hello)"
cargo run -p minictr --locked -- ps --store "$STORE"
cargo run -p minictr --locked -- stop --store "$STORE" "$id"
```

```text
INSTANCE	PID	IMAGE	STATE	AGE
i-<pid>	<pid>	hello	stale	<経過時間>
i-<pid>
```

detached実行にはsupervisorが居ないため、ゲストが書いたExit frameは誰も読まず、終了コードはホストへ届かない。
`guest-hello`のようにすぐ終わるゲストではQEMUも終了しているため`ps`は`stale`を出し、`stop`はstate fileだけを回収する。
ゲストが動き続けている間は`live`を出し、`stop`がQEMUの終了まで行う。
ゲスト出力はpayload directoryの`uart.log`へ落ち、`stop`がそのdirectoryごと回収する。

### imageの持ち出しと取り込み

登録したimageは`image export`でファイルへ、`image export-oci`でOCI Image Layoutへ書き出す。
どちらもbundleのバイト列をそのまま保持し、`image import`と`image import-oci`で同じdigestのまま戻せる。

```sh
cargo run -p minictr --locked -- image export --store "$STORE" hello --output ./hello.mcb
cargo run -p minictr --locked -- image export-oci --store "$STORE" hello --output ./hello-oci
cargo run -p minictr --locked -- image import --store "$STORE" hello-copy ./hello.mcb
```

## コマンドと終了コード

`minictr`が実装するコマンドは`run`、`ps`、`stop`、`doctor`、`image build`、`image import`、`image export`、`image list`、`image inspect`、`image remove`、`image prune`、`image export-oci`、`image import-oci`、`image pull-oci`、`help`、`--version`である。
構文と解決の詳細は[第9章](docs/guide/09-minictr-run.md)を参照する。

```text
usage: minictr run [--detach] [--store PATH] [--kernel PATH] [--timeout-ms N] [--memory MIB] [--cpus N] IMAGE
usage: minictr ps [--store PATH]
usage: minictr stop [--store PATH] [--timeout-ms N] i-<pid>
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
`--version`は`minictr 1.0.0`を標準出力へ出して0で終わる。
`--store`と`--kernel`を省略した値は環境変数`MINICTR_STORE`、`MINICTR_KERNEL`、なければ`$HOME/.minicontainer`以下から解決する。
`--timeout-ms`の既定値は5000である。

scriptが依存してよいのは終了コードと標準出力の契約行だけであり、標準エラー出力の診断文言は契約に含めない。

| 終了コード | 意味 |
| --- | --- |
| 0から255 | ゲストの終了コードをそのまま返す |
| 2 | 使い方の誤り（未知のコマンド、引数不足、不正な値） |
| 125 | ホスト側の失敗。`image build`の失敗、storeの解決失敗、QEMUの失敗、タイムアウト、SIGINTとSIGTERMによる中断を含む |
| 1 | `doctor`の診断失敗だけに使う |

### imageの操作

`image build`は静的RISC-V 64 ELFからMiniBundleを構築し、指定したタグでstoreへ登録する。
`image import`は配布されたMiniBundleファイルを検証してから同じ形で登録し、manifestの名前は書き換えない。
`image export`はタグまたは`sha256:`付きdigestで解決したbundleを検証して`--output`へ書き出す。出力先の上書きはしない。
`image list`はタグの一覧、`image inspect`は`tag`、`name`、`digest`、`args`、`elf-bytes`の5行を出す。
`image remove`は指定タグだけを削除して`IMAGE removed`と出し、blobと他のタグは保持する。
`image prune`は未参照blobの検出だけが既定動作であり、`--force`の指定時だけ削除する。削除前に参照を再確認し、部分失敗は個別に報告する。

`image export-oci`はstoreのimageをOCI Image Layoutのdirectoryへ書き出し、`image import-oci`はlayoutを検証してstoreへ登録する。
`image pull-oci`はregistryからdigest pinで匿名pullし、指定したタグで登録する。
pullはHTTPSだけを使い、認証はしない。
参照はtagではなくdigest pinだけを受け入れ、redirectは5回まで、接続10秒と要求60秒のtimeout、manifestとconfigは64 KiB、layerは6 MiBの上限で取得する。

```sh
cargo run -p minictr --locked -- image pull-oci --store "$STORE" hello registry.example.com:5000/demo/hello@sha256:<64桁の小文字16進数>
```

詳細は[第12章](docs/guide/12-oci-image-spec.md)と[MiniBundleとOCIの対応](docs/reference/minibundle-oci-mapping.md)を参照する。

### 実行と後始末

`run`はゲスト出力をdecodeされ次第、標準出力と標準エラー出力へ区別して逐次表示する。
stdout、stderr、診断の合計は表示済みも含めて1 MiBが上限であり、超過はホスト側の失敗として扱う。
実行中のCtrl-C（SIGINT）とSIGTERMは`minictr`が捕捉してQEMUのprocess groupへ転送し、2秒のgraceののち必要ならSIGKILLで回収する。
中断されたrunは後始末を経て終了コード125で終わる。
ホストの停止や捕捉できない`SIGKILL`による強制終了では、後始末を実行できない場合がある。

`ps`はrunが記録したinstanceを`live`、`stale`、`corrupt`の状態つきで一覧する。
`stop`はpid identityを照合してからQEMUのprocess groupへSIGTERMを送り、2秒のgrace（`--timeout-ms`で変更）の後も残ればSIGKILLし、payload directoryとstate fileを回収する。
形式は[instance state](docs/reference/instance-state.md)を参照する。

## 保証しない範囲

- Docker API互換、Linuxアプリケーション互換、OCI runtime互換。OCI対応はMiniBundle用artifactの配布形式（export、import、匿名pull）に限る。
- ゲストへのネットワーク、永続ボリューム、複数image実行。ネットワークはregistry pullのHTTPS clientだけである。
- 未信頼コードを扱うマルチテナント分離とguest escapeの防止。
- `minicontainer-*` crateのRust API。利用者が依存してよい公開面は`minictr` binaryだけである。
- Windowsでの動作。

安全境界の前提は[脅威モデル](docs/reference/threat-model.md)、非保証の全体は[互換性と移行の方針](docs/reference/compatibility.md)を参照する。

## 検証

公開前検証の入口は次の三つである。

```sh
cargo xtask setup
cargo xtask check-host
cargo xtask check
```

`check`はrustfmt、Markdownリンク、公開対象ファイル、crateごとのClippyと単体テスト、固定seedのbounded fuzz smoke、同梱ゲストのビルド、lockfileを使ったworkspaceのビルド、実QEMU end-to-end検証を18段階で実行する。
`check-host`は実QEMU検証を除く17段階を実行する。
CIはUbuntu 24.04で`setup`と`check`に加えて配布archiveのsmoke、OCI interop、互換性fixtureを検証し、Apple SiliconのmacOSで`check-host`を実行する。

E2Eは固定リビジョンのminiOSカーネルをビルドし、同梱ゲストを公開CLIへ一続きで通して、標準出力、標準エラー出力、終了コード、QEMUの回収、一時ディレクトリーの後始末を確認する。
タイムアウト、不正フレーム、出力上限、SIGINT、SIGTERM、強制終了、detached、stdinの各経路に加えて、公開コマンドのsurface passと反復と中断のcleanup stress節を含む。
parserのfuzzは`cargo xtask fuzz --target <bundle|uart>`で実行し、findingは決定性のreplayで再現する。
詳細は[第11章](docs/guide/11-test-harness-gate.md)と[セキュリティテスト](docs/reference/security-testing.md)を参照する。

## 文書

| 目的 | 文書 |
| --- | --- |
| 段階的に学ぶ | [ガイド索引](docs/guide/README.md)（全12章） |
| 設計の全体像 | [アーキテクチャ](docs/design/architecture.md) |
| 公開契約と検証根拠 | [公開契約の監査表](docs/reference/public-contract.md) |
| 変更時の解釈基準 | [互換性と移行の方針](docs/reference/compatibility.md) |
| 安全境界 | [脅威モデル](docs/reference/threat-model.md)、[Security Policy](SECURITY.md) |
| 配布と導入 | [配布手順](docs/reference/releasing.md) |
| 版ごとの変更 | [v1.0.0 release notes](docs/reference/v1.0.0-release-notes.md)、[v0.1.0 release notes](docs/reference/v0.1.0-release-notes.md) |
| 到達した節目 | [ロードマップ](docs/reference/roadmap.md) |
| 開発への参加 | [CONTRIBUTING](CONTRIBUTING.md) |

## ライセンス

MiniContainerはMIT LicenseまたはApache License 2.0の条件で利用できる。
詳細は[LICENSE-MIT](LICENSE-MIT)と[LICENSE-APACHE](LICENSE-APACHE)を参照する。
SPDX表記は`MIT OR Apache-2.0`である。
