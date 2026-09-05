# OCI Image Specificationへ進む

MiniBundleを外部へ配布する形式を検討するには、OCIでの保存、配布、実行を分けて考える必要がある。
OCI互換は将来方向であり、現在の機能ではない。
制約は[脅威モデル](../reference/threat-model.md)を参照する。

## 現在の形式

MiniBundleはheader、manifest、ELFを一つのfileに固めた形式である。
SHA-256 digestで破損を検出し、同じdigestでcontent-addressed storeに置く。
tagは名前からdigestへの対応付けであり、配布の仕組みは持たない。
実行単位は一つの静的ELFであり、filesystem layerやimage configの概念はない。

## OCI imageとの差

OCI imageは、image manifest、image config、layer blobの集合をdigestで束ねた形式である。
layerはfilesystem差分の積み重ねであり、単一ELFの直接格納とは異なる。
configは実行条件（architecture、環境、入口など）を宣言し、registryが配布とtag管理を担う。
メディアタイプは内容の形式を示し、annotationは補足のメタデータを持つ。
仕様は[OCI Image Manifest](https://github.com/opencontainers/image-spec/blob/main/manifest.md)と[Filesystem Layer](https://github.com/opencontainers/image-spec/blob/main/layer.md)を参照する。

共通するのは、digestによるcontent addressingと、tagからdigestへの間接参照である。
storeの`images/sha256`と`tags`の配置は、この二点ではOCIの考え方と一致する。
ただし、MiniContainerのストアはOCI Image Layoutそのものではない。
OCIのblobのダイジェストは格納したバイト列から計算するため、ヘッダー内のdigest欄をゼロにして計算するMiniBundleのダイジェストをそのまま代用できない。

## 拡張を検討する際の選択肢

まず、MiniBundleを配布用アーティファクトとして格納するのか、ファイルシステムを持つコンテナイメージを扱うのかを決める。
生のMiniBundleに通常のファイルシステムlayerのメディアタイプを付けても、その形式に準拠したことにはならない。
アーティファクトとして格納する案では、独自形式に合うメディアタイプとmanifestの対応付けを設計する。

形式が決まったら、ローカルな保存形式の読み取りと検証、レジストリーからの取得を別々に検討できる。
これは検討順の例であり、採用済みの実装計画ではない。
OCI形式で配布できることと、Docker向けのLinuxアプリケーションをminiOS上で実行できることは別の条件である。

レジストリー取得に使うホスト側ネットワークと、ゲストへのネットワーク機能の提供も区別する。
ゲストのネットワーク隔離と本番用途の分離は保証しない。
拡張の各段階でも、bundle不正の起動前拒否と失敗の三分類は維持する。
