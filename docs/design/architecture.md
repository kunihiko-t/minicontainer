# MiniContainerアーキテクチャ

## 目的と現在地

MiniContainerは、miniOSをゲストカーネルとして使い、一つのRISC-V 64アプリケーションを一つのQEMU仮想マシンで実行する学習用ランタイムである。
現在はMiniBundle、content-addressed store、UART control frame decoder、QEMU backend、payload受け渡し、instance lifecycle、`minictr run`、開発ハーネスを実装している。
`minictr run hello`は静的にリンクしたRISC-V 64 ELFを一つのQEMU仮想マシンで起動し、標準出力、標準エラー出力、終了コード、タイムアウトをホストへ返す。

## ホストとゲストの責務

MiniContainerはbundleの構築と検証、image store、QEMU子process管理を担当する。
miniOSはSv39アドレス空間、ELF loader、U-mode実行とsystem callを担当する。
ELF mappingとuser pointer検証をホスト側へ複製しない。

## MiniBundle

MiniBundle v1は96byte header、UTF-8 manifest、zero padding、静的RISC-V 64 ELFを一つのfileへ格納する。
SHA-256 digestは破損検出とcontent addressingに使い、署名や配布元の認証を提供しない。

## Guest ABI

MiniContainerはtagで固定したminios-abi (`minios-abi-v0.2.0`) を利用し、boot header、manifest、UART control frame、system call番号を共有する。
実行kernelはminiOSの固定revision (`4865f9be97a6cdcd77c71e36b1ba426b49bd73d7`) からbuildする。
ホスト側の検証は、miniOSが行うguest側の再検証を省略する根拠にならない。

## ゲスト実行の流れ

`minictr`がstoreからbundleを解決し、runtimeが一時payloadを作ってQEMUを起動し、miniOSがELFをU-modeで実行する。
`write`と`exit`の結果はUART control frameでホストへ届き、runtimeがQEMUを回収する。
Exit frameの後、kernelはresource回収を検証し、成功markerをDiagnostic frameで送ってからshutdownする。
回収失敗やfatal trapはGuestError frameになり、runは型付き失敗になる。

## エラーと後始末

bundle不正はQEMU起動前に拒否する。
タイムアウト、出力上限超過、QEMU起動失敗、ゲスト実行失敗、プロトコル破損、アプリケーションの非0終了を区別する。
通常の成功経路とエラー経路で、子プロセスの回収と一時領域の削除を試み、後始末の失敗も報告する。
ホスト停止や強制終了では、後始末を実行できない場合がある。
イベントループとプロセス読み取りの二か所で期限を確認し、出力が流れ続けてもタイムアウトを検出する。
OSの入出力や後始末を含む、呼び出し全体の厳密な終了時刻は保証しない。

## 対象環境

主要な開発環境はApple Silicon搭載macOS、継続検証環境はUbuntu 24.04とApple Silicon搭載macOSである。
Windowsは対象外である。

## 保証しない範囲

MiniContainerは本番用のセキュリティー境界ではありません。
未信頼コードを扱うマルチテナント環境、Linux application互換、network、永続volume、性能SLAは保証しない。
