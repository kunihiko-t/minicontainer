# MiniContainerガイド

このガイドは、miniOSをゲストカーネルとして使うRISC-V 64ランタイムを段階的に学ぶための索引である。

## 現在読める実装範囲

現在のコードはFoundation、固定したGuest ABI、MiniBundle codecとstore、UART control frame decoder、release gateまでを実装している。
各章の本文は今後作成するため、次の一覧は完成済みの章へのlinkではなく、全12章の予定と到達点である。

QEMU backend、payloadの受け渡し、instance runtime、`minictr run`は未実装である。
そのため、現時点ではQEMU container runnerを起動できない。

## 全12章の予定

1. **コンテナ、仮想マシン、microVMの違い**：ホストとゲストの責務を分け、MiniContainerが採用する仮想マシン境界を説明できるようにする。
2. **開発環境と`cargo xtask`**：Rust、RISC-V target、QEMU、Gitを診断し、同じrelease gateをローカルとCIで実行できるようにする。
3. **静的ELFとアプリケーションABI**：ゲストが受け入れるELFとGuest ABIの条件を説明できるようにする。
4. **MiniBundleのmanifestとdigest**：canonical bundleを構築し、layout、入力上限、SHA-256 digestを検証できるようにする。
5. **Boot payloadをゲストへ渡す**：MiniBundleからboot payloadを作り、予約メモリーへ配置できるようにする。
6. **QEMU backend**：再現可能なcommand lineでQEMUを起動し、子processを管理できるようにする。
7. **UART frameと標準入出力**：分割されたcontrol frameをdecodeし、stdout、stderr、終了通知を区別できるようにする。
8. **Instance状態と後始末**：起動から終了までの状態遷移、timeout、一時領域のcleanupを実装できるようにする。
9. **`minictr run`を完成させる**：imageの解決からQEMUの終了までを一つのCLI flowとして実行できるようにする。
10. **失敗、timeout、診断log**：host error、guest failure、protocol破損を区別して調査できるようにする。
11. **テストハーネスと公開gate**：書式、文書、Clippy、単体試験、buildを同じcommandから順に検証できるようにする。
12. **OCI Image Specificationへ進む**：現在の単一file形式とOCI imageの差を整理し、次の拡張順を計画できるようにする。

## 現在のコードと章の対応

第2章の環境診断とrelease gate、第4章のMiniBundle codecとstore、第7章のhost decoder、第11章のhost側verificationを先に実装している。
これは各章のruntime到達点が完成したことを意味しない。
第5章、第6章、第8章から第10章までに必要なQEMUとguest実行の経路は将来の作業である。

## 設計資料

設計上の責務と到達点は[MiniContainerアーキテクチャ](../design/architecture.md)を参照する。
安全性の前提と保証しない範囲は[脅威モデル](../reference/threat-model.md)を参照する。
