# コンテナ、仮想マシン、microVMの違い

この章は、ホストとゲストの責務を分け、MiniContainerが採用する仮想マシン境界を説明できるようにする。
全体の設計はアーキテクチャ、前提と保証しない範囲は脅威モデルを参照する。

## 三つの実行形態

process分離のコンテナは、ホストと同じkernelを共有し、namespaceとcgroup相当の仕組みでprocessを区切る。
起動は速いが、kernelが単一の信頼基盤になる。

仮想マシンは、VMMが用意する仮想hardwareの上で独立したguest kernelを動かす。
guestは独自のaddress空間とsystem callを持つため、境界はhardware仮想化とguest kernelの実装に依存する。

microVMは、一つのapplicationのために最小化した仮想マシンである。
仮想hardwareとguest kernelを削り、起動の速さと見通しの良さを取る。
分離の強さは残した実装に依存するため、用途を限定して使う。

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
