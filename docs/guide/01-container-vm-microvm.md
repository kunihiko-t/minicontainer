# コンテナ、仮想マシン、microVMの違い

この章は、ホストとゲストの責務を分け、MiniContainerが採用する仮想マシン境界を説明できるようにする。
全体の設計はアーキテクチャ、前提と保証しない範囲は脅威モデルを参照する。

## 三つの実行形態

Linuxのプロセス分離型コンテナは、ホストと同じカーネルを共有し、namespaceで資源の見え方を分け、cgroupで資源の使用量を管理する。
コンテナごとにカーネルを起動せず、共有するカーネルを信頼する前提で動作する。

仮想マシンは、VMMが用意する仮想hardwareの上で独立したguest kernelを動かす。
ゲストは独自のカーネルとシステムコールを持つ。
仮想CPUの実行には、ハードウェア支援による仮想化やソフトウェアによるエミュレーションを使う。
MiniContainerはQEMUでRISC-Vをエミュレートするため、Apple Silicon上でもRISC-Vゲストを動かせる。

microVMは、仮想デバイスなどを絞った軽量な仮想マシンを指す。
たとえば[Firecracker](https://github.com/firecracker-microvm/firecracker)はデバイスモデルを最小限にし、起動時間とメモリー使用量の削減を目指している。
単一アプリケーションしか実行できないという定義ではなく、性能や分離の保証は実装によって異なる。
MiniContainerも責務を小さく分けて学ぶ設計だが、Firecrackerの性能や分離特性を備えているという意味ではない。

## MiniContainerの境界

MiniContainerは、一つのRISC-V 64 applicationを一つのQEMU仮想マシンで実行する。
guest kernelはminiOSであり、Sv39 address空間、ELF loader、U-mode実行、system callを担当する。
ホストはbundleの構築と検証、image store、QEMU子process管理を担当する。
ELF mappingとuser pointer検証をホスト側へ複製しない。

この分離点は教育用の実装境界であり、本番の封じ込め保証ではない。
QEMUとminiOSは信頼する計算基盤であり、どちらかの脆弱性は前提を崩す。
未信頼codeを扱うmulti-tenant環境、guest escapeの防止、network隔離は保証しない。

## 学習上の位置づけ

以降の章は、この境界に沿って責務を一つずつ追う。
第2章から第4章でhost側の入力と検証を固め、第5章から第9章でQEMUとguest実行の経路をつなげる。
第10章で失敗の切り分けを、第11章で検証のgateを、第12章で次の拡張方向を扱う。
