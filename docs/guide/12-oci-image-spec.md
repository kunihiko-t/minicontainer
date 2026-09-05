# OCI Image Specificationへ進む

この章は、現在の単一file形式とOCI imageの差を整理し、次の拡張順を計画できるようにする。
OCI互換は将来方向であり、現在の機能ではない。
制約と保証しない範囲は脅威モデルを参照する。

## 現在の形式

MiniBundleはheader、manifest、ELFを一つのfileに固めた形式である。
SHA-256 digestで破損を検出し、同じdigestでcontent-addressed storeに置く。
tagは名前からdigestへの対応付けであり、配布の仕組みは持たない。
実行単位は一つの静的ELFであり、filesystem layerやimage configの概念はない。

## OCI imageとの差

OCI imageは、image manifest、image config、layer blobの集合をdigestで束ねた形式である。
layerはfilesystem差分の積み重ねであり、単一ELFの直接格納とは異なる。
configは実行条件（architecture、環境、入口など）を宣言し、registryが配布とtag管理を担う。
media typeとannotationが各blobの解釈を定める。

共通するのは、digestによるcontent addressingと、tagからdigestへの間接参照である。
storeの`images/sha256`と`tags`の配置は、この二点ではOCIの考え方と一致する。
異なるのは、layer、config、registry配布の三点である。

## 拡張の順序

安定した後の拡張は、差の小さい順に進める。
まず形式の対応付けとして、MiniBundleを単一layer相当として読み替える。
次にimage manifestとconfigの読み取りを足し、OCI layoutの検証までをhostで行う。
最後にregistry配布の取得を検討する。

networkとproduction isolationは、OCI互換を含む後続作業でも保証しない。
拡張の各段階でも、bundle不正の起動前拒否と失敗の三分類は維持する。
