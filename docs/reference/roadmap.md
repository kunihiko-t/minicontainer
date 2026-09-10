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

## 開発マイルストーン

以降の開発は、ローカル操作、イメージ配布、実行制御、互換性の順に進める。
各マイルストーンのIssueはGitHubで管理し、このページには機能の境界と依存順を残す。
期日は品質を下げる根拠にならないため、検証可能な完了条件を優先し、現時点では設定しない。

### v0.2.0 Local Workflow

v0.2.0では、既存のMiniBundleとQEMU実行方式を変えず、日常的なローカル操作を整える。

1. `minictr doctor`でQEMU、kernel、store、必要なtoolを診断する。
2. MiniBundleをfileからstoreへimportする。
3. storeのMiniBundleをfileへexportする。
4. blobを残したままtagだけを削除する`image remove`を追加する。
5. 未参照blobを確認してから削除する`image prune`を追加する。
6. 配布archiveの展開、導入、実行をCIのsmoke testで確認する。

`image remove`と`image prune`を分ける理由は、tagの削除とcontent-addressed blobの削除では回復可能性が異なるためである。
`image prune`は候補表示とdry-runを先に実装し、参照中のblobを削除しない検査をrelease gateへ追加する。

### v0.3.0 OCI Distribution

v0.3.0では、Linux application互換を追加せず、イメージの保存形式と配布だけをOCIへ接続する。

1. MiniBundleとOCI Image Layoutの対応、digest、media type、architectureの扱いを設計する。
2. storeのイメージをOCI Image Layoutへexportする。
3. OCI Image LayoutからMiniBundleを構築してstoreへimportする。
4. OCI registryからdigest指定で匿名pullする。
5. ORASまたはSkopeoとの相互運用をfixtureとE2Eで確認する。

OCI形式で配布できても、Docker向けLinux applicationをminiOSで実行できるわけではない。
[第12章](../guide/12-oci-image-spec.md)で説明する二つの互換性を分けたまま実装する。

### v0.4.0 Runtime Control

v0.4.0では、一回の同期実行だけを扱うCLIから、実行中のguestを観測して制御できるランタイムへ進める。

1. stdoutとstderrを上限付きで逐次表示する。
2. 疑似TTYを使わないstdin転送を追加する。
3. SIGINTとSIGTERMの転送、QEMU回収、一時領域の後始末を一つの契約へ揃える。
4. QEMUのmemory量とvCPU数を公開CLIから指定できるようにする。
5. 実行instanceの状態形式と`minictr ps`を設計する。
6. 状態形式を利用して`run --detach`と`minictr stop`を追加する。

逐次出力は現在の合計1 MiB上限を無効にせず、表示済みbyteを含む総量の扱いを先に決める。
detached実行は所有者不明のQEMUを残さない状態形式と回収手順が決まってから実装する。

### v1.0.0 Stable Learning Runtime

v1.0.0では、学習用マイクロVMランタイムとして利用者が更新時の影響を判断できる公開契約を固定する。

1. MiniBundle formatとGuest ABIの互換性、廃止、移行方針を定める。
2. bundle parserとUART protocol decoderへfuzz testを追加する。
3. 長時間実行、繰り返し起動、割り込み時のcleanupをstress testで確認する。
4. release artifactへ検証可能なprovenanceを付与する。
5. CLI、公開Rust API、脅威モデル、release gateを一括して監査する。

v1.0.0はDocker互換や本番向けマルチテナント分離の宣言ではない。
安定化する対象は、文書で公開したMiniBundle、Guest ABI、CLI、終了code、cleanupの契約である。

## マイルストーン間の依存関係

v0.2.0のimportとexportは、v0.3.0のOCI変換が利用するローカル入出力の境界になる。
v0.4.0のinstance状態とdetached実行は、既存のprocess lifecycleとcleanupを維持できることを前提にする。
v1.0.0の互換性方針は、それ以前の実装経験から安定させる契約を選ぶため、v0.2.0からv0.4.0より後に確定する。

各Issueの実装では、依存Issueを本文へ明記し、focused test、crate gate、`cargo xtask check-host`、必要な実QEMU E2Eを完了条件に含める。

## 将来候補

miniOS側の設計と実装を伴うnetwork、永続volume、Linux application互換は、上記マイルストーンへ含めない。
これらを開始するときは、MiniContainerだけで完結する変更とGuest ABIやkernelの変更を別Issueへ分ける。

未信頼codeを扱うマルチテナント用途とguest escape防止の保証も、現在の延長として宣言しない。
制約と保証しない範囲は[脅威モデル](threat-model.md)を参照する。
