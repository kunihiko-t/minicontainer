# コントリビュート

MiniContainerへの変更は、設計書で定義したホストとゲストの境界を維持する。

## 開発環境

Rust 1.98.0、RISC-V target、QEMU、Gitを用意する。
検査の入口は次のコマンドである。

```sh
cargo xtask setup
cargo xtask check
```

`setup`はRust、RISC-V target、QEMU、Gitのversionを変更せずに診断する。
`check`は依存関係を解決するすべてのCargo phaseを`--locked`で実行し、10段階の検査を最初の失敗で停止する。

## コメントと言語

公開するコメントと文書は日本語で書く。
用語、前提、保証範囲を曖昧にせず、実装済みの機能と将来の目標を区別する。

## ライセンス

コントリビューションはMIT LicenseまたはApache License 2.0の条件で提供することに同意したものとして扱う。
SPDX表記は`MIT OR Apache-2.0`である。

## 脆弱性の報告

未公開の脆弱性をpublic issueへ投稿しない。
GitHubのprivate vulnerability reportingを使う。
