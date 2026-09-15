# ロードマップ

実装済みの機能、依存するminiOSの機能、将来の拡張候補を示す。
起動手順は[README](../../README.md)、学習の順序は[ガイド索引](../guide/README.md)を参照する。

## 現在

### バンドルとGuest ABI

Cargo workspaceは公開に必要な文書とlocal linkを検査し、公開前の文書条件をgateに含めている。
MiniContainerは固定したminiOS Guest ABI (`minios-abi-v0.2.0`) を依存として取得し、BootHeader、manifest、UART control frameの定義をhost側の実装で消費している。
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
上限は表示済みbyteも含めた合計であり、超過時は`RunOutcome`を返さない。
E2Eの出力上限経路では、連打guestによる125終了、診断の一致、転送量の上限内、QEMU回収、一時directory cleanupを確認する。

### 配布

`v*`タグのpushでmacOS arm64用とLinux x86_64用の配布アーカイブを構築し、tagのGitHub Releaseへ添付する。
`minictr`、固定revisionのminiOS kernel、ライセンス、`MANIFEST.txt`、`SHA256SUMS`を含め、各アーカイブとchecksumに署名済みprovenanceを付ける。
手順は[配布手順](releasing.md)、機能と制約の一覧は[v0.1.0 release notes](v0.1.0-release-notes.md)を参照する。

### 学習ガイド

ガイド全12章の本文を用意している。
第1章から第11章は現在の実装、第12章はOCI対応の形式資料である。
第10章はhost error、guest failure、protocol破損の三分類と終了code、timeoutの二層強制、診断logの扱いを説明する。
第11章は`setup`と16段階の`check`と15段階の`check-host`、公開条件の検査、CIとの同一性を説明する。
第1章から第4章は境界、環境、ELFとGuest ABI、MiniBundleを、第12章はMiniBundleとOCI Image Layoutの対応と拡張順の計画を扱う。

## 開発マイルストーン

以降の開発は、ローカル操作、イメージ配布、実行制御、互換性の順に進める。
各マイルストーンのIssueはGitHubで管理し、このページには機能の境界と依存順を残す。
期日は品質を下げる根拠にならないため、検証可能な完了条件を優先し、現時点では設定しない。
[GitHub Roadmap Issue](https://github.com/kunihiko-t/minicontainer/issues/31)では、全マイルストーンの進捗を一覧できる。

### [v0.2.0 Local Workflow](https://github.com/kunihiko-t/minicontainer/milestone/1)

v0.2.0では、既存のMiniBundleとQEMU実行方式を変えず、日常的なローカル操作を整える。

1. [`feat(cli): add minictr doctor environment diagnostics`](https://github.com/kunihiko-t/minicontainer/issues/9)：`minictr doctor`でQEMU、kernel、store、必要なtoolを診断する。
2. [`feat(bundle): import MiniBundle files into the local store`](https://github.com/kunihiko-t/minicontainer/issues/10)：MiniBundleをfileからstoreへimportする。
3. [`feat(bundle): export stored images as MiniBundle files`](https://github.com/kunihiko-t/minicontainer/issues/11)：storeのMiniBundleをfileへexportする。
4. [`feat(store): remove tags without deleting blobs`](https://github.com/kunihiko-t/minicontainer/issues/12)：blobを残したままtagだけを削除する`image remove`を追加する。
5. [`feat(store): prune unreferenced blobs safely`](https://github.com/kunihiko-t/minicontainer/issues/13)：未参照blobを確認してから削除する`image prune`を追加する。
6. [`ci: smoke-test release archive installation and execution`](https://github.com/kunihiko-t/minicontainer/issues/14)：配布archiveの展開、導入、実行をCIのsmoke testで確認する。

`image remove`と`image prune`を分ける理由は、tagの削除とcontent-addressed blobの削除では回復可能性が異なるためである。
`image prune`は候補表示とdry-runを先に実装し、参照中のblobを削除しない検査をrelease gateへ追加する。

### [v0.3.0 OCI Distribution](https://github.com/kunihiko-t/minicontainer/milestone/2)

v0.3.0では、Linux application互換を追加せず、イメージの保存形式と配布だけをOCIへ接続する。

1. [`docs(oci): define the MiniBundle and OCI Image Layout mapping`](https://github.com/kunihiko-t/minicontainer/issues/15)：MiniBundleとOCI Image Layoutの対応、digest、media type、architectureの扱いを[対応文書](minibundle-oci-mapping.md)として定める。
2. [`feat(oci): export stored images as OCI Image Layout`](https://github.com/kunihiko-t/minicontainer/issues/16)：storeのイメージをOCI Image Layoutへexportする。
3. [`feat(oci): import OCI Image Layout into the MiniBundle store`](https://github.com/kunihiko-t/minicontainer/issues/17)：OCI Image LayoutからMiniBundleを構築してstoreへimportする。
4. [`feat(registry): pull anonymous images by digest`](https://github.com/kunihiko-t/minicontainer/issues/18)：OCI registryからdigest指定で匿名pullする。
5. [`test(oci): verify ORAS and Skopeo interoperability`](https://github.com/kunihiko-t/minicontainer/issues/19)：ORASまたはSkopeoとの相互運用をfixtureとE2Eで確認する。

OCI形式で配布できても、Docker向けLinux applicationをminiOSで実行できるわけではない。
[第12章](../guide/12-oci-image-spec.md)と[対応文書](minibundle-oci-mapping.md)で分けた二つの互換性を維持したまま実装する。

### [v0.4.0 Runtime Control](https://github.com/kunihiko-t/minicontainer/milestone/3)

v0.4.0では、一回の同期実行だけを扱うCLIから、実行中のguestを観測して制御できるランタイムへ進める。

1. [`feat(runtime): stream bounded stdout and stderr incrementally`](https://github.com/kunihiko-t/minicontainer/issues/20)：stdoutとstderrを上限付きで逐次表示する。
2. [`feat(runtime): forward non-TTY stdin to the guest`](https://github.com/kunihiko-t/minicontainer/issues/21)：疑似TTYを使わないstdin転送を追加する。
3. [`feat(runtime): unify signal forwarding and cleanup`](https://github.com/kunihiko-t/minicontainer/issues/22)：SIGINTとSIGTERMの転送、QEMU回収、一時領域の後始末を一つの契約へ揃える。
4. [`feat(cli): configure QEMU memory and vCPU limits`](https://github.com/kunihiko-t/minicontainer/issues/23)：QEMUのmemory量とvCPU数を公開CLIから指定できるようにする。
5. [`feat(runtime): persist instance state and add minictr ps`](https://github.com/kunihiko-t/minicontainer/issues/24)：実行instanceの状態形式と`minictr ps`を設計する。
6. [`feat(runtime): add detached run and minictr stop`](https://github.com/kunihiko-t/minicontainer/issues/25)：状態形式を利用して`run --detach`と`minictr stop`を追加する。

逐次出力は現在の合計1 MiB上限を無効にせず、表示済みbyteを含む総量の扱いを先に決める。
detached実行は所有者不明のQEMUを残さない状態形式と回収手順が決まってから実装する。

### [v1.0.0 Stable Learning Runtime](https://github.com/kunihiko-t/minicontainer/milestone/4)

v1.0.0では、学習用マイクロVMランタイムとして利用者が更新時の影響を判断できる公開契約を固定する。

1. [`docs: define compatibility, deprecation, and migration policies`](https://github.com/kunihiko-t/minicontainer/issues/26)：MiniBundle formatとGuest ABIの互換性、廃止、移行方針を定める。
2. [`test: fuzz MiniBundle and UART protocol parsers`](https://github.com/kunihiko-t/minicontainer/issues/27)：bundle parserとUART protocol decoderへfuzz testを追加する。
3. [`test: stress long-running, repeated, and interrupted cleanup`](https://github.com/kunihiko-t/minicontainer/issues/28)：長時間実行、繰り返し起動、割り込み時のcleanupをstress testで確認する。
4. [`ci: attach verifiable provenance to release artifacts`](https://github.com/kunihiko-t/minicontainer/issues/29)：release artifactへ検証可能なprovenanceを付与する。
5. [`audit: finalize the public CLI, API, threat model, and release gates`](https://github.com/kunihiko-t/minicontainer/issues/30)：CLI、公開Rust API、脅威モデル、release gateを一括して監査する。

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
