# ロードマップ

この文書は、MiniContainerで実装済みの範囲、依存するminiOSの到達点、次のcross-repository実装を区別して記録する。

## 現在

**FoundationとGuest ABI**の節目は完了した。

Cargo workspaceは公開に必要な文書とlocal linkを検査し、公開前の文書条件をgateに含めている。
MiniContainerは固定したminiOS Guest ABIを依存として取得し、BootHeader、manifest、UART control frameの定義をhost側の実装で消費している。
system call番号と呼び出し規約もGuest ABIに定義済みだが、runtimeはまだsystem callを実行せず、その定義を消費していない。

MiniBundleはcanonicalなbundleを構築してparseし、digestとmanifestを検証してcontent-addressed storeへimport、tag、resolveできる。
UART control frame decoderは分割入力を復元し、不正なheaderと64 KiBを超えるpayloadを拒否する。
`cargo xtask setup`は開発環境を変更せずに診断し、`cargo xtask check`は10段階の検査を実行する。
Ubuntu 24.04のCIはsetupと同じcheckを実行する。

依存先のminiOSでは、Sv39と実行前ELF loaderの節目が完了した。
miniOSはactiveなS-modeカーネルアドレス空間を使って起動し、静的RISC-V 64 ELFからinactiveな`LoadedImage`を構築して回収できる。
この到達点はMiniContainerがpage tableやELF loaderを実装したことを意味しない。
MiniContainerの現在の実装は、MiniBundle内のELFをminiOSへ渡さず、QEMU子processも起動しない。

QEMU host runtime、U-mode遷移、user trap context、`write`、`exit`、application lifecycle CLI、OCI、network、production isolationは未実装である。

## 次

次のcross-repository受け入れ単位は、miniOSでinactiveな`LoadedImage`をU-mode実行へ進め、最小の出力と終了を観測できる状態である。

miniOS側は次の順序で進める。

1. `LoadedImage`のentryとuser stackからU-modeの初期contextを作り、`sret`で実行を開始する。
2. U-modeからのtrapで全register、`sepc`、`sstatus`をuser trap contextへ保存する。
3. `write`がuser pointerのrangeとPTE権限を検査し、許可したbyteをUARTへ出力する。
4. `exit`が終了codeを保持し、address spaceとkernel stackの所有frameを回収する。

MiniContainer側はこの段階のQEMU host runtimeを設計し、UARTの出力と終了を既存Guest ABIのdecoderへ接続する。
ただし、最初の受け入れではtimeout、異常終了、cleanupを含むlifecycleを一つずつ観測可能にし、未検査の`minictr run`を先に公開しない。

### 受け入れ条件

- miniOSがU-modeのentryを実行し、S-mode専用pageへのaccessを拒否する。
- user trap contextが`write`と`exit`の前後で必要なregisterを保持する。
- `write`が正常なbufferを出力し、不正なuser pointerを拒否する。
- `exit`が終了codeをhostから区別できる形で伝え、所有frameを回収する。
- MiniContainerのhost試験がUART control frameとprocess終了を混同しない。
- 両repositoryの既存release gateが引き続き成功する。

## その後

payload統合では、MiniBundleの検証済みmanifestとELF範囲をboot payload予約領域へ配置する。
miniOSはそのELF byte sliceを既存の`ElfImage`、`LoadPlan`、`LoadedImage`の経路へ渡す。
MiniContainer内またはminiOS内に別のELF loaderを作り、同じvalidationとrollbackを再実装しない。

その後、QEMU子process、UART標準入出力、終了status、timeout、cleanupを一つのruntime lifecycleへ接続し、`minictr run`から呼び出す。
この経路の到達点は、一つのcontainerにつき一つのQEMU仮想machineでRISC-V 64 applicationを実行することである。

OCI compatibilityは、単一fileのMiniBundleとruntime lifecycleが安定した後の将来方向とする。
networkとproduction isolationはOCI compatibilityを含む後続作業でも保証しない。

制約と保証しない範囲は[脅威モデル](threat-model.md)を参照する。
