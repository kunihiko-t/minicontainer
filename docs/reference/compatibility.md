# 互換性と移行の方針

この文書は、MiniContainerの公開契約が変わるときの互換性、廃止、移行の共通方針を定める。
[#26](https://github.com/kunihiko-t/minicontainer/issues/26)で定義し、v1.0.0が固定する契約の解釈基準である。
各契約の技術詳細は対応する文書が正であり、この文書は変更時の扱いだけを定める。

## 対象契約とversion marker

MiniContainerが安定化の対象とする契約と、その変更を検出するmarkerは次の通りである。

| 契約 | version marker | 正の文書 |
| --- | --- | --- |
| MiniBundle format | boot headerの`abi_major`と`abi_minor`、manifestの`version`行 | [第4章](../guide/04-bundle-manifest-digest.md) |
| Guest ABI | Ready frameの`abi_major`と`abi_minor` | [第3章](../guide/03-static-elf-abi.md)、[第7章](../guide/07-uart-runtime.md) |
| CLI構文と終了code | releaseのtag version | [第9章](../guide/09-minictr-run.md)、[第10章](../guide/10-failure-diagnostics.md) |
| instance state schema | state file先頭の`minicontainer-state-vN`行 | [instance state](instance-state.md) |
| MiniBundleとOCIの対応 | media type名の`vN` | [OCI対応](minibundle-oci-mapping.md) |

`minicontainer-*`のRust crateは公開契約に含めない。
crates.ioへ公開しておらず、crate APIのsemver保証は提供しない。
利用者が依存してよい公開面は`minictr` binaryだけである。

## 契約ごとのbreaking change

利用者側の作業を必要とする変更をbreakingと定義し、それ以外を互換とする。

### MiniBundle format

breaking:`abi_major`の増加、header layoutの変更、digest計算領域の変更、manifest `version`行の値変更である。
これらは旧bytesを新しいhostが型付きerrorで拒否する方向の不一致であり、利用者はbundleをbuildし直す。

互換:`abi_minor`の増加である。
hostは自minor以下のbundleを受理するため、古いminorでbuildされたbundleは引き続きparse、実行、importできる。
逆方向は保証せず、自minorより新しいbundleは`UnsupportedMinor`として拒否する。

bundle digestはbyte列の関数であり、論理imageの同一性ではない。
同じELFとmanifestを別minorでbuildすると別bytes、別digestになる。
storeの同一性はdigestで決まるため、digestをartifactの固定IDとして長期保持する用途では、build時のABI minorを記録に残す必要がある。

### Guest ABI

breaking:`abi_major`の増加である。
guestとhostはmajorが一致しない相手を起動時に拒否し、利用者はguest kernelとimageを新しいABIでbuildし直す。

互換:`abi_minor`の増加である。
hostは`guest minor ≤ host minor`のguestを受理する。
minorで追加された機能はhandshakeのguest minorで可否を判別し、対応しないguestへは送らない。
stdin転送はminor 1以上でだけ有効であり、ABI v1.0のguestへstdin入力を要求したrunは型付きerrorで失敗する。

### CLI構文と終了code

breaking:subcommandやflagの削除と改名、終了codeの意味変更、stdout契約行の形式変更である。
stdout契約行は、`sha256:<hex>`のdigest行、detached runの`i-<pid>`行、`ps`の列構成を指す。

互換:subcommand・flagの追加、標準エラー出力への診断追加、`ps`への列追加である。
`ps`の列は末尾への追加だけを互換とし、既存列の削除や並べ替えはbreakingに分類する。

終了codeの契約は、guest終了codeの0から255の受け渡し、使い方の誤りの2、store解決失敗とruntime失敗の125、`doctor`診断失敗の1である。
scriptが依存してよいのは終了codeとstdout契約行だけであり、標準エラー出力の診断文言は契約に含めない。

### instance state schema

fieldの追加・削除・並べ替えはいずれもversion行を`vN`から`vN+1`へ上げる変更であり、同一version内での拡張は行わない。
versionが一致しないfileはcorruptとして扱い、version間の移行は提供しない。
stateは実行中だけ意味を持つ短命な記録であり、version不一致のfileは`ps`がcorruptとして表示し、`stop`は触れずに失敗する。
回収はinstance fileを消して再登録する手順であり、guest自体はorphanとして`stop`の対象外になる。

### MiniBundleとOCIの対応

media type名の`vN`で固定する。
対応の変更は新しい`vN+1`名を導入し、`v1`のtype名を再利用しない。
詳細は[OCI対応文書の版変更方針](minibundle-oci-mapping.md)が正である。

## サポート期間と廃止手順

サポート対象は最新releaseのみであり、旧release系統へのbackportは行わない。
修正はmainへ入り、次のreleaseへ乗る。

CLIの要素を廃止するときは、先にrelease notesと当該文書へ廃止予告を出し、少なくとも1回のreleaseをまたいでから削除する。
version markerを持つ契約（bundleの`abi_major`、state schema、media type）は猶予期間を設けず、新版への切り替えと同時に旧版を型付きerrorで拒否する。
bundleの`abi_minor`とguest ABIのminorは受理規則に従うため、これらの増加は切り替えを要求しない。

## 移行例

### ABI v1.0のbundleをv1.2のhostで使う

`image import`、`import-oci`、`run`はいずれもそのまま受理し、`image export`はbyte一致のbundleを返す。
移行作業は不要である。
`image build`でbuildし直すとcanonical bytesがv1.2になり、digestが変わる点だけに注意する。

### stdin対応のguestへ移す

guest applicationを`minios-abi`のv0.2.0以降へ再buildし、pinするkernel revをstdin対応版へ上げる。
ABI v1.0のguestは引き続き起動するが、stdin入力を伴うrunは起動前に型付きerrorで失敗する。

### state schemaのversion bump後に古いfileが残る

version不一致の`i-*.state`は`ps`がcorruptとして表示する。
`stop`はcorrupt fileに触れず125で失敗するため、利用者はfileを削除し、残ったQEMUはprocess listから対象を確認して手動で終了させる。

### CLI flagの廃止

仮に`--timeout-ms`を`--deadline-ms`へ改める場合、旧名は1回のreleaseにわたり受理を続け、docsとrelease notesで廃止予告を出す。
次のreleaseで旧名をusage error (終了code 2) として削除する。

## v1.0.0が保証しない範囲

- `minicontainer-*` crateのRust API。内部実装であり、利用者向けの公開面ではない。
- QEMU binaryとversionの互換。`doctor`が検査する下限を満たすQEMUでの実行だけを対象とする。
- temp dirの命名（`minicontainer-run-*`）、store内部のfile配置、instance file名の形式。
- detached実行が残す`uart.log`と`qemu.log`の内容と形式。検査用の副産物であり、出力契約ではない。
- 標準エラー出力の診断文言。
- manifest v2と`ProcExit` frameによる複数image実行。frame集合には含まれるが、現行runtimeでは受理しない。
- guest ABI minorが足りないときの個別機能。利用可否はhandshakeでnegotiateされ、不足は型付きerrorになる。
- Docker API互換、Linux application ABI、本番向けマルチテナント境界。

## 互換性を検査するfixtureとgate

| fixture・gate | 守る契約 |
| --- | --- |
| `tests/fixtures/compat/minibundle-abi-v1.0.mcb` | ABI v1.0のbundleを現行hostが受理する |
| `tests/fixtures/oci-interop/` | 外部toolのlayoutをbyte一致で往復する |
| protocol crateのgolden test | frame encodingのbyte列 |
| bundle crateのgolden test | canonical headerとdigest領域 |
| `cargo xtask check`の実QEMU E2E | 終了code、stdout契約行、cleanup |

## release時の確認

release notesは、含まれる契約変更をbreakingと互換に分類して記載する。
契約を変えるPRは、この文書または対象契約の文書の更新を同じPRへ含める。
tag前には`tests/fixtures/compat`のfixtureがCIで成功していることと、この文書の記述が実装と一致していることを確認する。
