# テストハーネスと公開gate

書式、文書、Clippy、単体試験、ビルド、実QEMUでの検証は、共通のテストハーネスから実行する。
検証の入口は`cargo xtask setup`と`cargo xtask check-host`と`cargo xtask check`の三つだけである。
`xtask`はこの三つに加えて、配布物を組み立てる`dist`とparserをfuzzする`fuzz`を受け付ける。
`setup`、`check-host`、`check`に引数を付けると型付きerrorになる。

## setupは診断だけを行う

`cargo xtask setup`は開発環境を変更せず、必要なtoolの有無とversionだけを診断する。
Rustは1.98.0 stableの完全一致を要求し、`riscv64gc-unknown-none-elf` targetの導入を確認する。
QEMUは8.2.0以上を要求し、Gitはversionの読み取りを確認する。
不足があれば、導入手順つきの診断を出して失敗する。

## checkは18段階を順に実行する

`cargo xtask check`は18段階の検査を番号順に実行し、最初の失敗で停止する。
`cargo xtask check-host`は実QEMUの最終段階を除く17段階を同じ順序で実行する。
依存関係を解決するすべてのCargo phaseは`--locked`で実行する。
各段階は`[n/18]`または`[n/17]`の開始行と経過秒つきの成否行を出し、最後に全体の要約を出す。

| 段階 | 検査 |
| --- | --- |
| 1 | `cargo fmt --all -- --check`による書式検査 |
| 2 | Markdown内のローカルリンクの検査 |
| 3 | 必須文書、禁止内容、ワークフロー、コミットのメールアドレスの検査 |
| 4〜8 | bundle、protocol、runtime、minictr、xtaskの順にClippyを実行 |
| 9〜13 | 同じcrate順に単体テストを実行 |
| 14 | 固定seedによるbounded fuzz smoke (bundle 500、uart 2000入力) |
| 15 | 同梱ゲスト例 (guest-hello) のrelease build |
| 16 | stdin echoのゲスト例 (guest-echo) のrelease build |
| 17 | `cargo build --workspace --locked`によるビルド |
| 18 | 実QEMUによるエンドツーエンド検証 |

Clippyには`--all-targets --locked -- -D warnings`、単体テストには`--locked`を指定する。

安い検査を先に置き、失敗箇所を一つに絞るのが順序の意図である。
`--locked`は、lockfileと食い違う依存解決での検証成功を許さない。

## テストを定期的に棚卸しする

テストは件数ではなく、検出できる退行によって保守する。
マイルストーンの完了時と、既存テストを包含する単体テスト、結合テスト、E2Eを追加したときに棚卸しする。

次の条件に該当し、固有の退行を検出しないテストは削除候補になる。

- 同じ入力、分岐、公開契約を別のテストと重複して確認している。
- 利用者が観測できる振る舞いではなく、変更可能な内部構造だけを固定している。
- 実装を意図的に変更したときだけ失敗し、誤った振る舞いでは失敗しない。
- 上位のテストに完全に包含され、失敗箇所の切り分けや高速な局所検証にも役立たない。

テスト時間や件数だけを理由に削除しない。
境界値、失敗経路、cleanup、セキュリティー、再現性を個別に担うテストは、E2Eと一部が重なっていても残す。
上位のテストが失敗原因を特定できない場合は、同じ契約を短時間で切り分ける単体テストにも役割がある。

削除前に、そのテストが検出していた退行を一文で特定する。
残るテストが同じ退行を検出することを確認し、focused testと`cargo xtask check-host`を実行する。
QEMU、process回収、guest protocolに関わる変更では`cargo xtask check`も実行する。
段階数や公開手順が変わった場合は、この章、README、CIを同じ変更で更新する。

## 公開条件の検査

第3段階では、必須文書とREADMEのライセンス表記、セキュリティー境界を保証しない旨の記載を確認する。
Git管理下のテキストには、特定のローカルパス、メールドメイン、秘密鍵やトークンの既知パターンがないかを調べる。
すべての秘密情報を検出できる検査ではないため、差分の目視確認も必要である。

ワークフローでは、外部のGitHub ActionsがコミットSHAに固定されていることなどを確認する。
Dependabot設定も検査対象になる。
Git履歴の作者とcommitterのメールアドレスは公開プロフィール上のidentityになり得るため、このgateでは制限しない。
詳細は[公開条件の実装](../../xtask/src/publication.rs)で確認できる。

内容検査は`git ls-files`に列挙されるファイルが対象であり、未追跡の新規ファイルを含まない。
追加する文書は内容を確認してから`git add`し、その後に検査する。
検査は作業ツリーの内容を読むため、ステージ後に編集した場合はコミット対象との差も確認する。

## CIは同じgateを実行する

Ubuntu 24.04のCIは、`cargo xtask setup`に続けて`cargo xtask check`を実行する。
`check`の後、Ubuntu CIは配布archiveのsmokeを実行する。`cargo xtask dist`でarchiveを組み立て、checksumの検査と展開を経て、展開した`minictr`で同梱ゲストを起動して`hello from guest`と終了code 42を確認する。
`check`の後、Ubuntu CIはOCI相互運用の検証を実行する。fixtureのchecksum検査、ORASとSkopeoの導入、fixtureのimport、再exportの一致、実行可能guestの外部tool複写とimportと実行を確認する。
Apple SiliconのmacOS CIはQEMUを導入せず、`cargo xtask check-host`を実行する。
同じ検査を共有するが、OS、QEMUのバージョン、権限、ネットワークなどの差によって結果は変わり得る。
ローカルでの成功に加え、対象コミットのCI結果も確認する。
Pull Requestの統合条件は[コントリビュート](../../CONTRIBUTING.md)を参照する。

## E2Eは最終段階である

第18段階は、pin留めしたminiOS revisionからguest kernelをbuildし、同梱ゲストを公開CLIの`image build`、`image inspect`、`run`へ一続きで通して、標準出力、標準エラー出力、終了code 42を確認する。
resources経路では`--memory 256 --cpus 2`の非既定値でも同じ出力と終了codeになり、QEMUと一時領域の残留がないことを確認する。
timeout経路、malformed-frame経路、出力上限経路では、非0終了に加えてQEMUと一時領域の残留がないことを確認する。
timeout経路とmalformed-frame経路では失敗時にguest出力を転送しない。
出力上限経路では拒否までに1 MiB以内の出力が逐次転送されるため、125終了と診断の一致に加えて転送量が1 MiB以内であることを確認する。
割り込み経路では、回転中のguestへprocess group宛のSIGINTを送り、`minictr`が125で終わりQEMUと一時領域の残留がないことを確認する。
SIGTERM経路では`minictr`のPIDだけへsignalを送り、125終了と同じ非残留を確認する。
crash経路では`minictr`をSIGKILLしてcleanupを回避させ、孤児QEMUを殺したあと`ps`がそのinstanceをstaleと表示することを確認する。
detached経路では`run --detach`が`i-<pid>`だけを出力し、`stop`がQEMUとpayloadとstate fileを回収することを確認する。
stdin echo経路では、pipeしたbinary入力がguest-echoからそのまま戻り、EOF後に終了code 42になることを確認する。
プロセス残存検査には`ps`が必要であり、実行制限のあるサンドボックスでは権限エラーになることがある。
通常は`target/e2e/minios`へ固定リビジョンを取得するため、初回はネットワーク接続も必要になる。
既存の取得済みソースを使う場合は、`MINICTR_E2E_MINIOS_DIR`に固定リビジョンと一致する、未変更のチェックアウトを指定する。
失敗の分類は[第10章](10-failure-diagnostics.md)を参照する。

## E2E末尾のstress節

単発経路のあと、一つの共有storeに対して反復と中断のscenarioをbounded回だけ実行する。
`hello`と`spin`の二つのtagを同じstoreへ登録し、全scenarioが同じ`run/` state directoryを使う。
反復数は次のとおりであり、全体で数十秒に収まる。

- foreground: 5回。`run hello`が終了code 42を返す。
- timeout: 5回。`run spin`が800 msの期限で125を返す。
- SIGINTとSIGTERM: 各3回。回転中のrunへsignalを送り、125とsignal名の診断を確認する。
- detach: 3回。`run --detach`のidを`stop`で回収する。
- crash: 2回。`minictr`をSIGKILLし、孤児QEMUを殺したあと`stop`がstale instanceを回収する。

各iterationのあとでQEMU processと`minicontainer-run-*` payload directoryの残留を検査する。
失敗は`stress <scenario> iteration <n> failed: <原因>`の形で、scenario名・iteration番号・残留対象つきで報告する。
節の終わりに、共有storeの`run/`にinstance state fileが残っていないこと、`ps`がinstance行を出さないこと、QEMUとpayloadの差分が開始時snapshotからゼロであることを確認する。
