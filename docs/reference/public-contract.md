# 公開契約の監査表

この文書は、MiniContainerの公開契約ごとに保証内容とその検証根拠を対応付けた監査表である。
[#30](https://github.com/kunihiko-t/minicontainer/issues/30)の監査としてv1.0.0で固定したものであり、各契約の技術詳細は対応する文書が正である。
破壊的変更の分類と移行手順は[互換性と移行の方針](compatibility.md)を、安全境界は[脅威モデル](threat-model.md)を参照する。

## 監査の規則

- 「検証根拠」は、その保証を実際に検査するtest、fixture、またはgateを指す。unit testと実QEMU E2Eを区別して記す。
- 検証根拠を持たない保証は書かない。保証できないものは[非保証の一覧](#明示的な非保証)へ記す。
- 契約を変えるPRは、この表と検証根拠を同じPRで更新する。

## CLI command

公開commandは`minictr` binaryだけである。
command名と引数構文は`minictr help`のusage行が正であり、tracked Markdown中のcommand記述は`cli.rs`のtestが`help()`と双方向に照合する。

| command | 保証 | 検証根拠 |
| --- | --- | --- |
| `run` | guestの標準出力をstdoutへ透過し、guestの終了codeをそのまま返す。`--detach`ではinstance id `i-<pid>`を1行で返す | 実QEMU E2E (happy/timeout/sigint/sigterm/crash/detached/stdin/stress)、unit test |
| `ps` | state fileのあるinstanceを`INSTANCE`/`IMAGE`/`STATUS`列で出す。liveとstaleを区別する | 実QEMU E2E (detached、stale行、crash回復、stress終了時の空確認)、unit test |
| `stop` | pid identityを照合してからQEMUを終了させ、payload directoryとstate fileを回収してinstance idを出す。staleとcorruptを区別する | 実QEMU E2E (detached回収、crash回収、stress)、unit test |
| `doctor` | QEMU version、kernel file、store rootを診断し、`ok`または`fail`と`summary`を出す | CI smoke (ubuntu、macOS)、unit test |
| `image build` | ELFと`--arg`からcanonical bundleをbuildし、digest行`sha256:<hex>`を出す | 実QEMU E2E、CI OCI interop step、unit test |
| `image import` | bundle fileを検証してstoreへ登録する | CI互換性fixture step (ABI v1.0受理)、unit test |
| `image export` | storeのbundleをbyte一致でfileへ出す | 実QEMU E2Eのcli surface pass、unit test |
| `image list` | 登録済みimageを列挙する | 実QEMU E2Eのcli surface pass、unit test |
| `image inspect` | imageのdigestとmanifest情報を出す | 実QEMU E2E、CI OCI step、unit test |
| `image remove` | imageをstoreから消す | 実QEMU E2Eのcli surface pass、unit test |
| `image prune` | 参照されないimageを消す。`--dry-run`と`--force`を持つ | 実QEMU E2Eのcli surface pass、unit test |
| `image export-oci` | storeのbundleをOCI layoutへbyte一致で出す | CI OCI interop step (oras/skopeo往復、byte一致の`cmp`)、unit test |
| `image import-oci` | OCI layoutのbundleを検証してbyte一致でstoreへ登録する | CI OCI interop step、unit test |
| `image pull-oci` | 匿名のdigest pin pullでregistryからbundleを取る | unit test。registry依存のため実実行のsmoke対象外 |
| `help` | 全commandのusage行を出す | 実QEMU E2Eのcli surface pass、unit test |
| `--version` | `minictr <version>`を出す | release archive smoke (ubuntu)、unit test |

## 終了code

| code | 意味 | 検証根拠 |
| --- | --- | --- |
| 0から255 | guestの終了codeの透過 | 実QEMU E2E (42の確認)、unit test |
| 2 | 使い方の誤り (未知command、引数不足、不正値) | unit test |
| 125 | host側の失敗。store解決失敗、runtime失敗、timeout、中断を含む | 実QEMU E2E (timeout、signal、crash、stress)、unit test |
| 1 | `doctor`の診断失敗のみ | CI smoke、unit test |

## stdout契約行

scriptが依存してよいstdout行は、`sha256:<hex>`のdigest行、detached runの`i-<pid>`行、`ps`の列構成である。
`ps`の列は末尾への追加だけを互換とする。
標準エラー出力の診断文言は契約に含めない。
検証根拠は実QEMU E2Eとunit testの出力assertionである。

## MiniBundle format

canonical header layout、digest計算領域、manifest `version`行を固定する。
hostは`abi_major`一致かつ`abi_minor`以下のbundleを受理し、それより新しいminorは`UnsupportedMinor`で拒否する。
importとimport-ociは検証済みのlayer bytesをそのまま格納し、storeとの往復でbundle bytesを変えない。

| 検証根拠 | 保証 |
| --- | --- |
| bundle crateのgolden test | header layoutとdigest領域のbyte列 |
| `tests/fixtures/compat/minibundle-abi-v1.0.mcb` | ABI v1.0 bundleの現行hostでの受理とdigest pin |
| `tests/fixtures/oci-interop/` | 外部tool layoutとのbyte一致の往復 |
| fuzz smokeとcorpus replay | 任意byte列への型付きerror、panicなし |

## Guest ABI

hostとguestは`abi_major`の一致を必須とし、hostは`guest minor ≤ host minor`を受理する。
stdin転送はminor 1以上のguestでだけ有効であり、ABI v1.0のguestへstdin入力を要求したrunは起動前に型付きerrorで失敗する。
manifest v2と`ProcExit` frameはframe集合に含まれるが現行runtimeでは受理しない。

| 検証根拠 | 保証 |
| --- | --- |
| protocol crateのgolden test | frame encodingのbyte列 |
| 実QEMU E2E (stdin echo、malformed frame) | handshake、stdin frame、異常frameの拒否 |
| fuzz smokeとcorpus replay | 任意byte列のdecoderへの耐性 |

## instance state schema

state file先頭の`minicontainer-state-vN`行でversionを識別し、version不一致はcorruptとして扱う。
同一version内の拡張は行わず、version間の移行は提供しない。
corrupt fileは`ps`が表示し、`stop`は触れずに125で失敗する。

| 検証根拠 | 保証 |
| --- | --- |
| unit test | schemaの書き込み、読み出し、corrupt識別 |
| 実QEMU E2E (crash、stress) | stale回収とstate fileの累積漏れなし |

## QEMU lifecycleとcleanup

QEMUは固定引数のprocess groupで起動し、SIGINTとSIGTERMを転送する。
中断されたrunは2秒のgraceののちSIGKILLで回収し、終了code 125で終わる。
成功、失敗、中断、crashのいずれの経路でも、QEMU processとpayload directoryに残滓を残さない。

| 検証根拠 | 保証 |
| --- | --- |
| 実QEMU E2Eのstress節 (5 scenario、21 iteration) | 反復、timeout、SIGINT、SIGTERM、detached stop、crash後の残滓なし |
| 各caseのbaseline差分 | QEMU pidとpayload directoryの増減ゼロ |

## 対応hostとtool

- 対象guestはRISC-V 64のstatic ELFのみである。
- 必要toolはRust 1.98.0、`riscv64gc-unknown-none-elf` target、QEMU 8.2.0以上、Gitである。
- 継続検証の対象hostはUbuntu 24.04とApple Silicon搭載macOSであり、前者は`cargo xtask check`全18段階、後者は`check-host`の17段階をCIで通す。
- 実QEMU E2Eが動作するのは`qemu-system-riscv64`が利用できるhostだけである。macOS CIは実機検証を持たず、E2EはUbuntu CIと開発者のlocalで担保する。
- その他のLinuxやWindows hostは検証対象外であり、動作は保証しない。

## 公開fileの検査

`cargo xtask check`のpublication policyは、gitのtracked file全体を走査し、local pathとlocal URL、個人mail、秘密鍵、GitHub token、AWS access keyを拒否する。
必須の公開fileの存在、workflowのpinと権限、依存関係の更新方針、version整合を検査する。
検証根拠はpublication policy自体とそのunit testであり、毎回のgateで実行される。

## 明示的な非保証

互換性文書の[v1.0.0が保証しない範囲](compatibility.md#v100が保証しない範囲)に加えて、次を保証しない。

- `image pull-oci`の実registryに対するsmoke。匿名digest pin pullのunit testだけを根拠とし、外部serviceの可用性はgateに含めない。
- 検証対象外hostでの動作。上記の対応hostだけをCIで確認する。
- guest ABI minorが足りないときの個別機能。利用可否はhandshakeでnegotiateされ、不足は型付きerrorになる。
- `cargo xtask`のcommand面。開発と配布のための内部toolであり、利用者向けの公開契約に含めない。

## 文書中のcommandのsmoke test

READMEとguideに記した`minictr` commandは、次の2経路で実行可能性を担保する。

- `cli.rs`の`documented_commands_match_the_public_help_surface`が、tracked Markdownの全command記述を`help()`のusage行と双方向に照合する。記述のないcommand、綴りの違うcommandを機械的に検出する。
- 実QEMU E2Eのcli surface passが、QEMUを必要としない公開command (`version`、`help`、`image list`、`image export`、`image import`、`image remove`、`image prune`) を実storeへ1回ずつ実行する。`pull-oci`はregistry依存のため対象外である。
