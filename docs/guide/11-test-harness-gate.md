# テストハーネスと公開gate

この章は、書式、文書、Clippy、単体試験、build、実機検証を同じcommandから順に検証する方法を説明する。
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

1. `cargo fmt --all -- --check`で書式を検査する。
2. 文書内のlocal Markdown linkの到達を検査する。
3. 公開条件（必須file、禁止内容、workflow、identity）を検査する。
4. から8. crateごとのClippyを`--all-targets --locked -- -D warnings`で実行する。順序はbundle、protocol、runtime、minictr、xtaskである。
9. から13. crateごとの単体試験を`--locked`で実行する。順序はClippyと同じである。
14. `cargo build --workspace --locked`でworkspace全体をbuildする。
15. 実QEMU end-to-end検証を実行する。

安い検査を先に置き、失敗箇所を一つに絞るのが順序の意図である。
`--locked`は、lockfileと食い違う依存解決での検証成功を許さない。

## 公開条件の検査

第3段階は、公開に耐える状態を五つに分けて検査する。
必須file（license二種、`SECURITY.md`、`CONTRIBUTING.md`、`README.md`、設計、索引、脅威モデル）の存在。
`README.md`にSPDX表記とsecurity境界の非保証文が含まれること。
Git管理下のtree全体に、local pathとURL、私物email、秘密鍵、token類が含まれないこと。
workflowの参照actionがimmutableな参照であることと、Dependabot設定が管理下にあること。
HEAD履歴全体のauthorとcommitterがnoreply形式だけを使うこと。

## CIは同じgateを実行する

Ubuntu 24.04のCIは、`cargo xtask setup`に続けて`cargo xtask check`を実行する。
localとCIでcommandが同一のため、localの緑はCIの緑と一致する。
Pull Requestの統合条件（検証済みheadのfast-forwardなど）は`CONTRIBUTING.md`を参照する。

## E2Eは最終段階である

第15段階は、pin留めしたminiOS revisionからguest kernelをbuildし、`minictr run hello`の標準出力、標準エラー出力、終了code 42を確認する。
timeout経路とmalformed-frame経路では、非0終了に加えてQEMUと一時領域の残留がなく、happy-pathの内容が混入しないことを確認する。
失敗の分類は第10章を参照する。
