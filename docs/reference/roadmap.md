# ロードマップ

実装済みの機能、依存するminiOSの機能、将来の拡張候補を示す。
起動手順は[README](../../README.md)、学習の順序は[ガイド索引](../guide/README.md)を参照する。

## 現在

### バンドルとGuest ABI

Cargo workspaceは公開に必要な文書とlocal linkを検査し、公開前の文書条件をgateに含めている。
MiniContainerは固定したminiOS Guest ABI (`minios-abi-v0.1.1`) を依存として取得し、BootHeader、manifest、UART control frameの定義をhost側の実装で消費している。
system call番号と呼び出し規約もGuest ABIに定義済みであり、runtimeは`write`と`exit`の結果をUART control frameで受け取る。

MiniBundleはcanonicalなbundleを構築してparseし、digestとmanifestを検証してcontent-addressed storeへimport、tag、resolveできる。
同梱の最小ゲスト例はsourceから静的RISC-V 64 ELFへbuildでき、E2Eのhappy pathとquick startの入力になる。
UART control frame decoderは分割入力を復元し、不正なheaderと64 KiBを超えるpayloadを拒否する。
`cargo xtask setup`は開発環境を変更せずに診断し、`cargo xtask check`は16段階の検査を実行する。
Ubuntu 24.04のCIはsetupと同じcheck、Apple SiliconのmacOS CIはQEMUを除く15段階のcheck-hostを実行する。

### QEMU上でのゲスト実行

開発上の節目「M1」は、このゲスト実行機能を指す。AppleのM1チップ限定という意味ではない。

依存先のminiOSでは、Sv39、実行前ELF loader、U-mode実行、`write`、`exit`、終了後のresource回収の節目が完了した。
実行kernelは固定revision (`9be99255a59d58d19db25b835af0e28a8d2a4036`) からbuildする。
この到達点はMiniContainerがpage tableやELF loaderを実装したことを意味しない。
MiniContainerは検証済みbundleをboot payload予約領域へ渡し、QEMU子process、UART標準入出力、終了status、timeout、cleanupを一つのruntime lifecycleへ接続し、`minictr run`から呼び出す。
一つのcontainerにつき一つのQEMU仮想machineでRISC-V 64 applicationを実行できる。

実QEMU end-to-end検証はrelease gateの最終段階として実行する。
同梱ゲストを公開CLIの`image build`、`image inspect`、`run`へ一続きで通して、標準出力、標準エラー出力、終了code 42、QEMU回収、一時directory cleanupを確認する。
timeout、malformed-frame、出力上限、割り込みの失敗経路では非0終了と残留の不在も確認する。

### 出力上限

stdout、stderr、diagnosticsの合計蓄積量に1 MiBの上限を設け、超過は型付きerror（host error）としてrunを失敗させる。
失敗時は蓄積済みの出力を転送しない。
E2Eの出力上限経路では、連打guestによる125終了、診断の一致、QEMU回収、一時directory cleanupを確認する。

### 配布

`v0.1.0`タグのpushでmacOS arm64用とLinux x86_64用の配布アーカイブを構築し、`minictr`、固定revisionのminiOS kernel、ライセンス、導入手順を含める。
手順は[v0.1.0配布手順](releasing.md)、機能と制約の一覧は[v0.1.0 release notes](v0.1.0-release-notes.md)を参照する。

### 学習ガイド

ガイド全12章の本文を用意している。
第1章から第11章は現在の実装、第12章は未実装のOCI対応を検討するための資料である。
第10章はhost error、guest failure、protocol破損の三分類と終了code、timeoutの二層強制、診断logの扱いを説明する。
第11章は`setup`と16段階の`check`と15段階の`check-host`、公開条件の検査、CIとの同一性を説明する。
第1章から第4章は境界、環境、ELFとGuest ABI、MiniBundleを、第12章はOCI imageとの差と拡張順の計画を扱う。

## 拡張候補

OCI対応は将来の拡張候補であり、採用する保存形式と実装順は未決定である。
[第12章](../guide/12-oci-image-spec.md)では、配布形式への対応とLinuxアプリケーションの実行互換を区別している。
ゲストのネットワーク機能と本番用途の分離は、現在の保証範囲に含まれない。

制約と保証しない範囲は[脅威モデル](threat-model.md)を参照する。
