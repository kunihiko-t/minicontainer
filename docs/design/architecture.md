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

MiniContainerはtagで固定したminios-abi (`minios-abi-v0.1.1`) を利用し、boot header、manifest、UART control frame、system call番号を共有する。
実行kernelはminiOSの固定revision (`9be99255a59d58d19db25b835af0e28a8d2a4036`) からbuildする。
ホスト側の検証は、miniOSが行うguest側の再検証を省略する根拠にならない。

## M1の実行の流れ

`minictr`がstoreからbundleを解決し、runtimeが一時payloadを作ってQEMUを起動し、miniOSがELFをU-modeで実行する。
`write`と`exit`の結果はUART control frameでホストへ届き、runtimeがQEMUを回収する。
Exit frameの後、kernelはresource回収を検証し、成功markerをDiagnostic frameで送ってからshutdownする。
回収失敗やfatal trapはGuestError frameになり、runは型付き失敗になる。

## エラーと後始末

bundle不正はQEMU起動前に拒否する。
timeout、QEMU起動失敗、guest failure、protocol破損、applicationの非0終了を区別し、すべての終了経路で子processと一時領域を回収する。
全体の期限はevent loopとprocess読み取りの二層で強制し、出力量にかかわらず効く。

## 対象環境

主要な開発環境はApple Silicon搭載macOS、継続検証環境はUbuntu 24.04である。
Windowsは対象外である。

## 保証しない範囲

MiniContainerは本番用のセキュリティー境界ではありません。
未信頼コードを扱うマルチテナント環境、Linux application互換、network、永続volume、性能SLAは保証しない。
