# テストハーネスと公開gate

書式、文書、Clippy、単体試験、ビルド、実QEMUでの検証は、共通のテストハーネスから実行する。
検証の入口は`cargo xtask setup`と`cargo xtask check`の二つだけである。
`xtask`が受け付けるcommandはこの二つであり、引数を付けると型付きerrorになる。

## setupは診断だけを行う

`cargo xtask setup`は開発環境を変更せず、必要なtoolの有無とversionだけを診断する。
Rustは1.98.0 stableの完全一致を要求し、`riscv64gc-unknown-none-elf` targetの導入を確認する。
QEMUは8.2.0以上を要求し、Gitはversionの読み取りを確認する。
不足があれば、導入手順つきの診断を出して失敗する。

## checkは15段階を順に実行する

`cargo xtask check`は15段階の検査を番号順に実行し、最初の失敗で停止する。
依存関係を解決するすべてのCargo phaseは`--locked`で実行する。
各段階は`[n/15]`の開始行と経過秒つきの成否行を出し、最後に全体の要約を出す。

| 段階 | 検査 |
| --- | --- |
| 1 | `cargo fmt --all -- --check`による書式検査 |
| 2 | Markdown内のローカルリンクの検査 |
| 3 | 必須文書、禁止内容、ワークフロー、コミットのメールアドレスの検査 |
| 4〜8 | bundle、protocol、runtime、minictr、xtaskの順にClippyを実行 |
| 9〜13 | 同じcrate順に単体テストを実行 |
| 14 | `cargo build --workspace --locked`によるビルド |
| 15 | 実QEMUによるエンドツーエンド検証 |

Clippyには`--all-targets --locked -- -D warnings`、単体テストには`--locked`を指定する。

安い検査を先に置き、失敗箇所を一つに絞るのが順序の意図である。
`--locked`は、lockfileと食い違う依存解決での検証成功を許さない。

## 公開条件の検査

第3段階では、必須文書とREADMEのライセンス表記、セキュリティー境界を保証しない旨の記載を確認する。
Git管理下のテキストには、特定のローカルパス、メールドメイン、秘密鍵やトークンの既知パターンがないかを調べる。
すべての秘密情報を検出できる検査ではないため、差分の目視確認も必要である。

ワークフローでは、外部のGitHub ActionsがコミットSHAに固定されていることなどを確認する。
Dependabot設定と、HEADからたどれるコミットの作者、コミッターのnoreply形式も検査対象になる。
詳細は[公開条件の実装](../../xtask/src/publication.rs)で確認できる。

内容検査は`git ls-files`に列挙されるファイルが対象であり、未追跡の新規ファイルを含まない。
追加する文書は内容を確認してから`git add`し、その後に検査する。
検査は作業ツリーの内容を読むため、ステージ後に編集した場合はコミット対象との差も確認する。

## CIは同じgateを実行する

Ubuntu 24.04のCIは、`cargo xtask setup`に続けて`cargo xtask check`を実行する。
同じ検査を共有するが、OS、QEMUのバージョン、権限、ネットワークなどの差によって結果は変わり得る。
ローカルでの成功に加え、対象コミットのCI結果も確認する。
Pull Requestの統合条件は[コントリビュート](../../CONTRIBUTING.md)を参照する。

## E2Eは最終段階である

第15段階は、pin留めしたminiOS revisionからguest kernelをbuildし、`minictr run hello`の標準出力、標準エラー出力、終了code 42を確認する。
timeout経路、malformed-frame経路、出力上限経路では、非0終了に加えてQEMUと一時領域の残留がなく、失敗時にguest出力を転送しないことを確認する。
割り込み経路では、回転中のguestへprocess group宛のSIGINTを送り、`minictr`が125で終わりQEMUと一時領域の残留がないことを確認する。
プロセス残存検査には`ps`が必要であり、実行制限のあるサンドボックスでは権限エラーになることがある。
通常は`target/e2e/minios`へ固定リビジョンを取得するため、初回はネットワーク接続も必要になる。
既存の取得済みソースを使う場合は、`MINICTR_E2E_MINIOS_DIR`に固定リビジョンと一致する、未変更のチェックアウトを指定する。
失敗の分類は[第10章](10-failure-diagnostics.md)を参照する。
