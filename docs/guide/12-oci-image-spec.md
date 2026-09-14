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
