# コントリビュート

MiniContainerへの変更は、設計書で定義したホストとゲストの境界を維持する。

## 開発環境

Rust 1.98.0、RISC-V target、QEMU、Gitを用意する。
検査の入口は次のコマンドである。

```sh
cargo xtask setup
cargo xtask check-host
cargo xtask check
```

`setup`はRust、RISC-V target、QEMU、Gitのversionを変更せずに診断する。
`check`は依存関係を解決するすべてのCargo phaseを`--locked`で実行し、18段階の検査を最初の失敗で停止する。
`check-host`は実QEMUの最終段階を除く17段階を実行する。

## テストの保守

マイルストーンの完了時と、既存テストと同じ退行をより広い範囲で検出するテストを追加したときに、重複したテストを棚卸しする。
固有の契約、境界値、失敗の切り分けを担わないテストは、残るテストで同じ退行を検出できることを確認してから削除する。
判断基準と削除手順は[テストハーネスと公開gate](docs/guide/11-test-harness-gate.md#テストを定期的に棚卸しする)に定める。

## セキュリティテスト

MiniBundle parserとUART decoderの変更では、境界テストに加えてfuzz harnessを実行する。

```sh
cargo xtask fuzz --target bundle
cargo xtask fuzz --target uart
```

findingがあると終了code 1になり、再現commandが表示される。
findingの入力は縮小して`xtask/corpus/<target>/`へ追加し、corpus replayで固定する。
検出器、制限、corpus保存方針は[セキュリティテスト](docs/reference/security-testing.md)に定める。

## コメントと言語

公開するコメントと文書は日本語で書く。
用語、前提、保証範囲を曖昧にせず、実装済みの機能と将来の目標を区別する。

## Pull Requestの統合

Pull RequestはCIとレビューの入口として使う。
検証済みのPull Requestは、GitHubのmerge操作または`gh pr merge`で統合できる。
作者とcommitterのidentityはGit履歴として公開されるため、各contributorがGitHub公開プロフィールを含めて適切なidentityを選ぶ。
公開条件の検査はGit履歴のメールアドレスを制限しない。

統合前に`origin/main`をfetchし、Pull Requestのbaseが移動していないことを確認する。
続いて、`cargo xtask check`とrequired checkが成功していることを確認する。

## Dependabotの更新

DependabotのPull Requestも、通常のPull Requestと同じレビューと検証を通して統合する。

## ライセンス

コントリビューションはMIT LicenseまたはApache License 2.0の条件で提供することに同意したものとして扱う。
SPDX表記は`MIT OR Apache-2.0`である。

## 脆弱性の報告

未公開の脆弱性をpublic issueへ投稿しない。
GitHubのprivate vulnerability reportingを使う。
