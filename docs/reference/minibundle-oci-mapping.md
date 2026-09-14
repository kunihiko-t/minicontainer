# MiniBundleとOCI Image Layoutの対応

この文書は、MiniBundleとOCI Image Layoutの正準対応を定義する。
[#15](https://github.com/kunihiko-t/minicontainer/issues/15)で定義し、[#16](https://github.com/kunihiko-t/minicontainer/issues/16)のexport、[#17](https://github.com/kunihiko-t/minicontainer/issues/17)のimport、[#18](https://github.com/kunihiko-t/minicontainer/issues/18)のregistry pullがこの対応に従う。
学習用の解説は[第12章](../guide/12-oci-image-spec.md)を参照する。
OCI側の用語（manifest、descriptor、layout）は[OCI Image Spec](https://github.com/opencontainers/image-spec)を参照する。

## 目的と対象範囲

MiniBundleを標準toolで保存、複写、配布できるようにし、MiniContainerのstoreとの往復でbundle bytesを変えない。
対象はOCI Image Layout 1.0系のdirectory配置（`oci-layout`、`index.json`、`blobs/`）と、その内容を指すdigest chainである。

対象外は、Docker runtime互換、Linux root filesystemの実行、registry認証、tar圧縮、複数platformのindex、registryへのpushである。
OCI形式で配布できることと、Docker向けLinux applicationをminiOSで実行できることは別の条件であり、後者は扱わない。

## 設計判断

MiniBundle全体を一つのlayer blobに載せ、image manifestのenvelopeで包む。
layer分割（ELFをlayer、manifestをconfigへ分離）は採用しない。
分割はimport時の再構築でfieldの欠落や順序の差を生むが、全体格納はround-tripをbyte一致で保証できる。

artifact manifest（`artifactType`付き）は採用しない。
[#19](https://github.com/kunihiko-t/minicontainer/issues/19)のORASとSkopeoの相互運用では、image manifestの扱いが最も枯れているenvelopeを選ぶ。
payloadの独自性はmedia typeで表し、未知の形式をLinux containerとして実行する経路は作らない。

生のMiniBundleに通常のfilesystem layerのmedia typeを付けることはしない。
形式に準拠しないmedia type表示は、toolに誤った解釈をさせる。

## Media typeとversioning

| 用途 | media type |
| --- | --- |
| image index | `application/vnd.oci.image.index.v1+json` |
| image manifest | `application/vnd.oci.image.manifest.v1+json` |
| image config | `application/vnd.minicontainer.image.config.v1+json` |
| MiniBundle layer | `application/vnd.minicontainer.bundle.v1+mcb` |

`+mcb`はopaqueなMiniBundle bytesであることを示す。
`artifactType`は使わない。
`oci-layout`の`imageLayoutVersion`は`1.0.0`だけを受け入れる。

`v1`はこの対応の版である。
対応を変えるときは新しいmedia type名を導入し、`v1`のtype名を再利用しない。
importは`v1`のtype名だけを受け入れ、未知の版は型付きerrorで拒否する。

## Layoutの配置

```
layout/
  oci-layout
  index.json
  blobs/sha256/<indexが指すmanifestのdigest>
  blobs/sha256/<configのdigest>
  blobs/sha256/<MiniBundle layerのdigest>
```

`oci-layout`のbytesは次の33 bytesで固定である。

```json
{"imageLayoutVersion":"1.0.0"}
```

`index.json`はmanifest記述を一つだけ持つ。
manifest記述は`platform`を必須とし、`architecture`は`riscv64`、`os`は`minios`だけを受け入れる。
`os`の`minios`は意図的な独自値であり、Linuxではない。
importは`linux`を含む他の値を拒否する。

`blobs/sha256/`直下の余分なblobは無視する。
blobの参照は`sha256:<小文字hex64桁>`の形式だけを受け入れ、path traversal（`..`、絶対path、symlinkの追従）は拒否する。

## Canonical JSON

exportは次の規則でJSON bytesを生成し、同じ入力から同じbytesを作る。

- keyの順序はこの文書の例の通り固定する。
- 区切りは`,`と`:`だけにし、空白と末尾改行を付けない。
- 文字列は`"`と`\`をescapeし、制御文字は小文字hexの`\u00XX`でescapeする。
- `/`はescapeせず、非ASCIIはUTF-8のまま置く。
- 数値は負でない整数の10進表記にし、先行zero、小数、指数を使わない。
- `schemaVersion`は数値の`2`にする。

canonical形はexportの要件である。
importは格納bytesのdigestを先に検証し、JSONの書式ゆれはparserに任せる。
ただし重複keyは拒否し、未知のfieldは無視する。
未知のfieldを無視するのはOCIの前方互換の慣行に従うためであり、未知のmedia typeやarchitectureの受け入れを意味しない。

## Fieldの対応

| MiniBundleのfield | OCI上の表現 | 備考 |
| --- | --- | --- |
| manifestのname | layer blob（正）とconfigの`Entrypoint[0]`（写し） | 不一致は拒否する |
| manifestのargs | layer blob（正）とconfigの`Cmd`（写し） | 不一致は拒否する |
| ELF bytes | layer blob | opaqueに格納する |
| header全体 | layer blob | opaqueに格納する |
| bundle digest | 派生値 | 検証後に再計算する |
| storeのtag | CLI引数だけ | layoutから引き継がない |
| annotation | 無視する | exportは付けない |

configの`Entrypoint`は要素を一つだけ持ち、`Cmd`は空を含む配列にする。
`created`、`author`、`history`は付けない。
`rootfs`は`{"type":"layers","diff_ids":["sha256:<layerのdigest>"]}`にし、圧縮しないため`diff_ids`はlayer blobのdigestと等しい。
exportはannotationを付けず、`org.opencontainers.image.ref.name`も付けない。
importは`ref.name`と`title`を含むannotationを無視し、tagは`image import`のCLI引数から取る。

## Config、manifest、indexの形状

configは次のkeyだけを持ち、順序は固定である。

```json
{"architecture":"riscv64","os":"minios","config":{"Entrypoint":["hello"],"Cmd":[]},"rootfs":{"type":"layers","diff_ids":["sha256:6c710671215c4929fa600956d236ae82df1f3a9f05f0b125c3fb0feed9096136"]}}
```

manifestはconfig記述を一つ、layer記述を一つだけ持つ。

```json
{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.minicontainer.image.config.v1+json","size":197,"digest":"sha256:3f19ea537fd3b4cdee56a34be0d9c7f90b1472d3e12c0e7aff3a505f7dfac98e"},"layers":[{"mediaType":"application/vnd.minicontainer.bundle.v1+mcb","size":136,"digest":"sha256:6c710671215c4929fa600956d236ae82df1f3a9f05f0b125c3fb0feed9096136"}]}
```

indexはmanifest記述を一つだけ持ち、`platform`を必須とする。

```json
{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","size":411,"digest":"sha256:f095c9356a85036bc3e4a08e562e6826321aedd3569aaa15e076a9639170995b","platform":{"architecture":"riscv64","os":"minios"}}]}
```

例の値は[name `hello`、引数なし、16 byteの固定ELF](#golden-fixture案)から計算した。
configは197 bytesでdigest `3f19ea537fd3b4cdee56a34be0d9c7f90b1472d3e12c0e7aff3a505f7dfac98e`、manifestは411 bytesでdigest `f095c9356a85036bc3e4a08e562e6826321aedd3569aaa15e076a9639170995b`、indexは292 bytesでdigest `29dd36cc92562f77bb648ee0ad74d4fe3cde3330f723fadd0f514c15309ceb54`である。

## Digestの領域

bundle digestとblob digestは定義が異なる。

- bundle digestは、digest欄をzeroにしたheaderと後続bytes全体へのSHA-256である。
- blob digestは、完成したMiniBundle file bytes全体へのSHA-256である。

例のbundle digestは`f8200f06e25dcb0f36a744b780c0fa1b40030c9a6bb9786b122cad44cad221ff`、layer blob digestは`6c710671215c4929fa600956d236ae82df1f3a9f05f0b125c3fb0feed9096136`であり、一致しない。
blob digestはbundle自身のdigest欄を含むため、両者を代用できない。

descriptorの`digest`と`size`は、参照先blobのbytesに対して検証する。
storeのcontent addressingはbundle digestだけを使い、OCIのdigestは輸送用としてstoreに残さない。
import後にstoreが持つdigestは、bundle検証から得たbundle digestである。

## Round-tripの保持表

| 情報 | 往復後 |
| --- | --- |
| header、manifest、ELFのbytes | byte一致で保持する |
| bundle digest | 再計算で一致する |
| nameとargs | bundle bytesの一部として保持する |
| storeのtag | 保持しない（CLI引数で付け直す） |
| annotationと未知のfield | 保持しない（無視する） |

## Size上限と安全性

- MiniBundle layerは8 MiB（`MAX_BUNDLE_LEN`）以内にする。
- config、manifest、indexの各JSONは64 KiB以内にする。
- importの検証順序は、size、digest、JSON parse、意味検証、MiniBundle parse、store書き込みの順にする。
- 検証がすべて通るまでstoreを変更しない。
- 参照pathはlayout rootの内側だけに解決し、symlinkは追従しない。

[#18](https://github.com/kunihiko-t/minicontainer/issues/18)のregistry pullは、同じ上限をdownloadの前後で守り、redirectは5回まで、schemeはhttpsだけ、認証情報は扱わない。
timeoutは必須とし、秒数は#18の実装が定めて文書に残す。
HTTP診断にsecretを含めない。

## 不正layoutの受け入れ表

| 条件 | 判定 |
| --- | --- |
| `oci-layout`の不在や版違い | 拒否する |
| `index.json`の不在、parse失敗、重複key | 拒否する |
| manifest記述が0個または2個以上 | 拒否する |
| manifest記述のmedia type違い | 拒否する |
| `platform`の不在、`riscv64`と`minios`以外 | 拒否する |
| config記述のmedia type違い | 拒否する |
| configのparse失敗、`Entrypoint`と`Cmd`の不一致 | 拒否する |
| layer記述が0個または2個以上、media type違い | 拒否する |
| descriptorのdigest不一致、size不一致 | 拒否する（store不変） |
| layer blobのMiniBundle parse失敗 | 拒否する（BundleErrorを返す） |
| 参照pathのtraversal、symlink | 拒否する |
| `blobs/sha256/`の余分なblob | 無視する |
| 未知のannotationとfield | 無視する |
| CLIのtag名不正 | 拒否する（storeのtag規則に従う） |

## Golden fixture案

[#16](https://github.com/kunihiko-t/minicontainer/issues/16)のgolden testと[#17](https://github.com/kunihiko-t/minicontainer/issues/17)のvalid fixtureは、次の固定入力から作る。

- name `hello`、argsなし、ELFは`0x00`から`0x0f`の16 bytes。
- このELFは合成であり、実行できるguestではない。digest計算の例示だけに使う。

bundleは136 bytes（header 96、manifest 21、padding 3、ELF 16）になる。
manifest bytesは`version=1\nname=hello\n`である。
bundle hexは次の通りである。

```text
4d494e4943545200010000006000000088000000000000006000000000000000150000000000000078000000000000001000000000000000f8200f06e25dcb0f36a744b780c0fa1b40030c9a6bb9786b122cad44cad221ff000000000000000076657273696f6e3d310a6e616d653d68656c6c6f0a000000000102030405060708090a0b0c0d0e0f
```

期待するlayoutは次の配置とdigest、sizeである。

```text
layout/
  oci-layout                                            33 bytes（固定）
  index.json                                            292 bytes、sha256:29dd36cc92562f77bb648ee0ad74d4fe3cde3330f723fadd0f514c15309ceb54
  blobs/sha256/f095c9356a85036bc3e4a08e562e6826321aedd3569aaa15e076a9639170995b  411 bytes（manifest）
  blobs/sha256/3f19ea537fd3b4cdee56a34be0d9c7f90b1472d3e12c0e7aff3a505f7dfac98e  197 bytes（config）
  blobs/sha256/6c710671215c4929fa600956d236ae82df1f3a9f05f0b125c3fb0feed9096136  136 bytes（MiniBundle layer）
```

bundle hexと各digestは`minictr image build`の実出力から計算した。
#16はこの入力からbyte一致のlayoutをassertし、#17はこのlayoutからbyte一致のbundleを復元する。

## 版の変更方針

`v1`の対応は固定する。
media type、必須field、canonical形、判定表のいずれかを変えるときは、新しい版のmedia type名を導入するIssueを作り、`v1`の実装とfixtureを壊さない。
互換性と廃止の宣言は[#26](https://github.com/kunihiko-t/minicontainer/issues/26)の方針に従う。
