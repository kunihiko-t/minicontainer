# ロードマップ

この文書は、MiniContainerで実装済みの範囲、依存するminiOSの到達点、次のcross-repository実装を区別して記録する。

## 現在

**FoundationとGuest ABI**の節目は完了した。

Cargo workspaceは公開に必要な文書とlocal linkを検査し、公開前の文書条件をgateに含めている。
MiniContainerは固定したminiOS Guest ABI (`minios-abi-v0.1.1`) を依存として取得し、BootHeader、manifest、UART control frameの定義をhost側の実装で消費している。
system call番号と呼び出し規約もGuest ABIに定義済みであり、runtimeは`write`と`exit`の結果をUART control frameで受け取る。

MiniBundleはcanonicalなbundleを構築してparseし、digestとmanifestを検証してcontent-addressed storeへimport、tag、resolveできる。
UART control frame decoderは分割入力を復元し、不正なheaderと64 KiBを超えるpayloadを拒否する。
`cargo xtask setup`は開発環境を変更せずに診断し、`cargo xtask check`は15段階の検査を実行する。
Ubuntu 24.04のCIはsetupと同じcheckを実行する。

**M1実行経路**の節目は完了した。

依存先のminiOSでは、Sv39、実行前ELF loader、U-mode実行、`write`、`exit`、終了後のresource回収の節目が完了した。
実行kernelは固定revision (`9be99255a59d58d19db25b835af0e28a8d2a4036`) からbuildする。
この到達点はMiniContainerがpage tableやELF loaderを実装したことを意味しない。
MiniContainerは検証済みbundleをboot payload予約領域へ渡し、QEMU子process、UART標準入出力、終了status、timeout、cleanupを一つのruntime lifecycleへ接続し、`minictr run`から呼び出す。
一つのcontainerにつき一つのQEMU仮想machineでRISC-V 64 applicationを実行できる。

実QEMU end-to-end検証はrelease gateの最終段階として実行する。
`minictr run hello`の標準出力、標準エラー出力、終了code 42、QEMU回収、一時directory cleanupを確認する。
timeoutとmalformed-frameの失敗経路では非0終了と残留の不在も確認する。

**失敗と診断の扱い**の節目は完了した。

ガイド全12章の本文が現在の実装に対応した。
第10章はhost error、guest failure、protocol破損の三分類と終了code、timeoutの二層強制、診断logの扱いを説明する。
第11章は`setup`と15段階の`check`、公開条件の検査、CIとの同一性を説明する。
第1章から第4章は境界、環境、ELFとGuest ABI、MiniBundleを、第12章はOCI imageとの差と拡張順の計画を扱う。

## 次

次の受け入れ単位はまだ定めていない。
将来方向はその後のOCI compatibilityである。

## その後

OCI compatibilityは、単一fileのMiniBundleとruntime lifecycleが安定した後の将来方向とする。
networkとproduction isolationはOCI compatibilityを含む後続作業でも保証しない。

制約と保証しない範囲は[脅威モデル](threat-model.md)を参照する。
