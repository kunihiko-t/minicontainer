# MiniContainer

MiniContainerは、miniOSをゲストカーネルとして使い、RISC-V 64アプリケーションをQEMU仮想マシンで実行する仕組みを段階的に学ぶための小型ランタイムである。
信頼できる開発者による個人開発、デモ、OS教材、ランタイム実験で使う実用的な学習用ランタイムを目標にする。

## 現在の実装範囲

現在はFoundation、Guest ABI、ホスト側codecの範囲を実装している。

- **Foundation**：Cargo workspace、Rust 1.98.0、RISC-V target、miniOS Guest ABI依存を固定している。
- **MiniBundle**：canonical bundleの構築と検証、SHA-256 digest、manifestの制限、content-addressed storeへのimport、tag、resolveを実装している。
- **UART control frame**：固定したGuest ABIに従う分割入力decoderを実装し、不正headerと64 KiBのpayload上限を検査する。
- **開発ハーネス**：環境診断と10段階のrelease gateを`cargo xtask`から実行できる。

QEMUの起動、miniOSへのpayload受け渡し、ゲストアプリケーションの実行、`minictr` CLIはまだ実装していない。
現在のリポジトリを「QEMUでコンテナを実行できるruntime」として使うことはできない。

## 到達点

M1では、`minictr run hello`が静的にリンクしたRISC-V 64 ELFを一つのQEMU仮想マシンで起動し、標準出力、標準エラー出力、終了コード、タイムアウトをホストへ返す到達点を実装する。
OCI互換は将来の方向であり、現在の機能ではない。

## 対応環境

主要な開発環境はApple Silicon搭載macOSである。
継続検証の対象はUbuntu 24.04である。
Windowsは対象外である。
RISC-V 64、QEMU、Git、Rust 1.98.0を使用する。

## 開発コマンド

現在の公開verification entry pointは次の二つである。

```sh
cargo xtask setup
cargo xtask check
```

`setup`はRust 1.98.0、`riscv64gc-unknown-none-elf`、QEMU 8.2.0以上、Gitを変更せずに診断する。
`check`はrustfmt、Markdown link、publication file、crateごとのClippyと単体試験、lockfileを使うworkspace buildを10段階で実行する。

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
