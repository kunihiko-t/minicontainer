# MiniContainerガイド

このガイドは、miniOSをゲストカーネルとして使うRISC-V 64ランタイムを段階的に学ぶための索引である。

## 章の一覧

第1章から第11章は現在の実装を説明し、第12章はOCI対応の形式資料である。
まず実行結果を確認したい場合は、[クイックスタート](../../README.md#クイックスタート)から始める。

1. **[コンテナ、仮想マシン、microVMの違い](01-container-vm-microvm.md)**：ホストとゲストの責務を分け、MiniContainerが採用する仮想マシン境界を説明できるようにする。
2. **[開発環境と`cargo xtask`](02-dev-environment-xtask.md)**：Rust、RISC-V target、QEMU、Gitを診断し、同じrelease gateをローカルとCIで実行できるようにする。
3. **[静的ELFとアプリケーションABI](03-static-elf-abi.md)**：ゲストが受け入れるELFとGuest ABIの条件を説明できるようにする。
4. **[MiniBundleのmanifestとdigest](04-bundle-manifest-digest.md)**：canonical bundleを構築し、layout、入力上限、SHA-256 digestを検証できるようにする。
5. **[Boot payloadをゲストへ渡す](05-boot-payload.md)**：MiniBundleからboot payloadを作り、予約メモリーへ配置できるようにする。
6. **[QEMU backend](06-qemu-backend.md)**：再現可能なcommand lineでQEMUを起動し、子processを管理できるようにする。
7. **[UART frameと標準入出力](07-uart-runtime.md)**：分割されたcontrol frameをdecodeし、stdout、stderr、終了通知を区別できるようにする。
8. **[Instance状態と後始末](08-instance-lifecycle.md)**：起動から終了までの状態遷移、timeout、一時領域のcleanupを実装できるようにする。
9. **[`minictr run`を完成させる](09-minictr-run.md)**：imageの解決からQEMUの終了までを一つのCLI flowとして実行できるようにする。
10. **[失敗、timeout、診断log](10-failure-diagnostics.md)**：host error、guest failure、protocol破損を区別して調査できるようにする。
11. **[テストハーネスと公開gate](11-test-harness-gate.md)**：書式、文書、Clippy、単体試験、buildを同じcommandから順に検証できるようにする。
12. **[OCI Image Specificationへ進む](12-oci-image-spec.md)**：MiniBundleとOCI Image Layoutの対応を理解し、export、import、registry pullの拡張順を計画できるようにする。

第5章から第9章までがQEMUとguest実行の経路に対応する。
第1章から第11章までの本文が現在のコードに対応し、第12章は[対応文書](../reference/minibundle-oci-mapping.md)と共に次の拡張の形式を定める。

## 実装を読むときの入口

| 対象 | コード | 対応する章 |
| --- | --- | --- |
| MiniBundleの構築とストア | [bundle](../../crates/bundle/src/lib.rs) | 第4章 |
| UARTフレームの復元 | [protocol](../../crates/protocol/src/lib.rs) | 第7章 |
| QEMU起動と実行後の後始末 | [runtime](../../crates/runtime/src/lib.rs) | 第5〜8章、第10章 |
| コマンドラインと終了コード | [minictr](../../crates/minictr/src/main.rs) | 第9章、第10章 |
| 環境診断と検証 | [xtask](../../xtask/src/lib.rs) | 第2章、第11章 |

## 設計資料

設計上の責務と到達点は[MiniContainerアーキテクチャ](../design/architecture.md)を参照する。
安全性の前提と保証しない範囲は[脅威モデル](../reference/threat-model.md)を参照する。
配布物の構築と確認は[配布手順](../reference/releasing.md)を参照する。
各公開契約の保証と検証根拠の対応は[公開契約の監査表](../reference/public-contract.md)を参照する。
v0.1.0の機能と制約の一覧は[v0.1.0 release notes](../reference/v0.1.0-release-notes.md)を参照する。
