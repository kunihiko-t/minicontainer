# OCI Image Specificationへ進む

MiniBundleを外部へ配布する形式として、OCI Image Layoutとの正準対応を使う。
対応の定義は[MiniBundleとOCI Image Layoutの対応](../reference/minibundle-oci-mapping.md)が正であり、この章は判断の理由と往復の見通しを説明する。
exportは[#16](https://github.com/kunihiko-t/minicontainer/issues/16)、importは[#17](https://github.com/kunihiko-t/minicontainer/issues/17)、registry pullは[#18](https://github.com/kunihiko-t/minicontainer/issues/18)が実装する。
OCI互換は配布形式の互換であり、Docker向けLinux applicationの実行互換ではない。
制約は[脅威モデル](../reference/threat-model.md)を参照する。

## 現在の形式

MiniBundleはheader、manifest、ELFを一つのfileに固めた形式である。
SHA-256 digestで破損を検出し、同じdigestでcontent-addressed storeに置く。
tagは名前からdigestへの対応付けであり、配布の仕組みは持たない。
実行単位は一つの静的ELFであり、filesystem layerやimage configの概念はない。
layoutと上限は[第4章](04-bundle-manifest-digest.md)を参照する。

## 対応の要点

MiniBundle全体を一つのlayer blobに載せ、image manifestで包む。
layer分割は採用しない。
全体格納は往復でbundle bytesを変えないが、分割は再構築で欠落の余地を残す。

envelopeはimage manifestを選び、artifact manifestは使わない。
ORASとSkopeoの相互運用では、枯れた経路を選ぶことが誤解釈の余地を狭める。
payloadの独自性は`application/vnd.minicontainer.bundle.v1+mcb`のmedia typeで表す。
生のMiniBundleにfilesystem layerのmedia typeを付けることはしない。

共通するのは、digestによるcontent addressingと、tagからdigestへの間接参照である。
storeの`images/sha256`と`tags`の配置は、この二点ではOCIの考え方と一致する。
ただし、MiniContainerのストアはOCI Image Layoutそのものではない。
OCIのblobのダイジェストは格納したバイト列から計算するため、ヘッダー内のdigest欄をゼロにして計算するMiniBundleのダイジェストをそのまま代用できない。
二つのdigest領域の違いは対応文書の計算例で確認する。

## exportの契約

`image export-oci`はstoreのtagを解決し、bundle bytesを不変のpayloadとしてlayoutへ包む。
`--output`の出力先が存在する場合は上書きせず失敗する。
同じ入力からはbyte一致のlayoutを作り、golden fixtureの値は対応文書の例と一致する。
成功行のdigestは`index.json`のSHA-256であり、CLIの形は[第9章](09-minictr-run.md)を参照する。

## importの契約

`image import-oci`はlayoutをstoreの外で検証し尽くしてから、正準のMiniBundle bytesだけをstoreへ置く。
検証の順序はsize、digest、JSON parse、意味検証、MiniBundle parseであり、失敗時はtagもblobも作らない。
indexの`platform`は任意のhintであり、不在でもconfigの宣言で検証する。
toolが複写時に付けるannotationは無視し、tagはCLI引数から付ける。
受け入れ判定の一覧は対応文書の判定表が正である。

## registry pullの契約

`image pull-oci`は匿名のdigest pin pullだけを行う。
参照は`host[:port]/repository@sha256:<hex>`の形であり、tagの解決と認証は対象外である。
取得順序はmanifest、config、layerであり、各blobはsizeとdigestの検証を通してから使う。
manifest自体のdigestはpinと照合し、不一致は取得の失敗にする。

転送の境界は次の通りである。

- schemeはHTTPSだけを使う。loopbackへのHTTPはfixture testだけの例外である。
- redirectは1 blobにつき5回まで追い、HTTPS以外への転送は取得前に拒否する。
- timeoutは接続10秒、1 blobの要求全体で60秒である。
- size上限はmanifestとconfigが64 KiB、layerが8 MiBであり、宣言と実測の両方で検査する。

blobはmemoryに読み、検証が通るまでstoreを変更しない。
失敗時は一時fileを作らず、storeにtagもblobも残さない。
診断は状態と成否だけを出し、URLやdigest、認証情報を含めない。
registryの応答は検証が通るまで信頼しない入力として扱う。

## 互換表

外部toolとの双方向交換を次の版で検証する。
fixtureは`tests/fixtures/oci-interop/`に置き、由来と更新手順は同梱のREADMEが正である。

| tool | 版 | 配布物の読み取り | tool出力の取り込み | 備考 |
| --- | --- | --- | --- | --- |
| ORAS | 1.3.4 | `cp`と`manifest fetch`と`blob fetch`で確認する | `cp`出力をimportして実行する | indexの`platform`を落とし`ref.name`を付ける。両方とも無視する |
| Skopeo | 1.24.0 | `copy`と`inspect --raw`で確認する | `copy`出力をimportして実行する | indexの`platform`とannotationを落とす |

CIのUbuntu jobは、ORASのpin版tarballのchecksum検証と導入、fixtureのchecksum検証、fixtureのimportとinspect、再exportのblob一致、実行可能guestのexportとtool複写とimportと実行を順に行う。
SkopeoはUbuntuのaptで導入し、版をlogに残す。pin済みfixtureのimportが決定版の主張を担う。

互換の主張は、blobのbyte一致とimportの成功と実行の成功だけである。
annotation、tag、未知のfieldの保持は主張しない。importは捨てる。
Docker向けLinux applicationの実行互換も主張しない。

## 往復の見通し

exportはstoreのbundle bytesを不変のpayloadとしてlayoutへ包む。
configはname、args、platformの写しであり、正はlayer内のbundleである。
importはsize、digest、JSON、意味、MiniBundle parseの順に検証し、すべて通るまでstoreを変更しない。
tagとannotationは往復で保持しない。
往復で保持する情報と捨てる情報の一覧は対応文書の保持表が正である。

localの入出力境界は、v0.2.0の`image import`と`image export`（[#10](https://github.com/kunihiko-t/minicontainer/issues/10)と[#11](https://github.com/kunihiko-t/minicontainer/issues/11)）が担う。
OCI変換はこの境界の上に載り、storeの検証規則を変えない。

## 拡張順の計画

形式が決まったら、ローカルな保存形式の読み取りと検証、レジストリーからの取得を別々に実装できる。
まず#16がexportで正準layoutを生成し、次に#17がimportで検証と復元を行い、最後に#18がregistryからの匿名pullを載せる。
[#19](https://github.com/kunihiko-t/minicontainer/issues/19)はORASまたはSkopeoとの双方向交換で解釈差を検出する。
これは採用済みの実装順であり、各Issueの完了条件が受け入れ基準になる。

OCI形式で配布できることと、Docker向けのLinuxアプリケーションをminiOS上で実行できることは別の条件である。
レジストリー取得に使うホスト側ネットワークと、ゲストへのネットワーク機能の提供も区別する。
ゲストのネットワーク隔離と本番用途の分離は保証しない。
拡張の各段階でも、bundle不正の起動前拒否と失敗の三分類は維持する。
