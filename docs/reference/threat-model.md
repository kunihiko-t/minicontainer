# 脅威モデル

## 対象

MiniContainerは、信頼できる開発者が作成したRISC-V 64アプリケーションを、学習、個人利用、デモ、実験のために実行することを対象にする。
保護したい対象は、ホストのファイル、ホストの認証情報、ホスト上の他のプロセス、QEMU子プロセスの終了処理である。
多弁なゲストによるホストメモリーの枯渇は、出力合計の1 MiB上限で抑える。
上限は終了前に表示したbyteも含めた合計であり、逐次転送しても緩まない。
表示先の停滞はevent loopへbackpressureをかけるが、全体の期限でrunを打ち切るため、遅いconsumerがQEMUを延命させることはない。

## 信頼する入力

miniOS Guest ABIの固定Git tag、実行kernelの固定revision、manifestおよびゲストELFを含むMiniBundleのバイト列を信頼する入力として扱う。
QEMU実行ファイルと開発者が指定する設定は、runtimeが扱う段階でも信頼する入力とする。
これらの入力を敵対者が作成または改変できる環境は、現在の対象外である。

## 信頼する計算基盤

QEMUとminiOSは信頼する計算基盤である。
ゲストの分離、ELF loaderの検証、user pointerの検査、system callの実行はminiOSに依存する。
ホスト側のdecoderとstore検証は破損検出のための層であり、ゲスト側の再検証を省略する根拠にはならない。
QEMUまたはminiOSの脆弱性はこのモデルの前提を崩す。
配布アーカイブ内のkernel binaryも固定revisionからbuildした同じminiOSであり、信頼前提は変わらない。

## OCI入力の扱い

OCI Image Layoutのdirectoryとregistryの応答は、検証が通るまで信頼しない入力として扱う。
信頼できるのは、sizeとdigestの検証、JSONと意味の検証、MiniBundle parseをすべて通過したbundle bytesだけである。
検証が一つでも失敗したらstoreを変更せず、tagも付けない。

layout内の参照pathはrootの内側だけに解決し、symlinkは追従しない。
JSON文書は64 KiB、MiniBundle layerは8 MiBの上限で読み、超過は拒否する。
registry pullはhttpsだけを使い、redirectは5回までとし、認証情報は扱わない。
loopbackへのhttpはfixture testだけの例外であり、CLIのpullはhttpsだけを使う。
接続10秒、1 blobの要求全体で60秒のtimeoutを付け、失敗時はstoreを変更しない。
診断は状態と成否だけを出し、URLやdigest、認証情報を含めない。
未知のannotationとfieldは無視するが、未知のmedia typeとplatformは拒否する。
対応の詳細は[MiniBundleとOCI Image Layoutの対応](minibundle-oci-mapping.md)を参照する。

## 仮想マシン境界

一つのRISC-V 64アプリケーションを一つのQEMU仮想マシンで実行する。
ゲストのU-modeとS-mode、QEMUの仮想マシンは、設計上の分離点である。
これらの分離点は教育用の実装境界であり、敵対的な入力に対する本番の封じ込め保証ではない。

## ホストファイル境界

store操作は、敵対的でないlocal filesystemを前提にする。
同時にstoreを変更する敵対的processからの封じ込めは保証しない。
symlink検査とstore内到達検査は、誤操作と破損への防御であり、並行する攻撃者への対策ではない。
`image prune`は削除直前に参照を再確認し、store側も参照中blobの削除を拒否するが、確認と削除の隙間をなくすものではない。
強制終了で残る一時fileは候補検出の対象外であり、内容の安全性に影響しない。

## 保証しない範囲

ホストの停止やランタイムの強制終了時に、子プロセスの停止と一時ファイルの削除が完了することは保証しない。
実行中のSIGINTとSIGTERMは保証の対象であり、`minictr`が捕捉してQEMUのprocess groupへ転送し、2秒のgraceののちSIGKILLで回収してから後始末を経て125で終わる。
`SIGKILL`だけは捕捉対象外のため、後始末なしに停止し子processや一時fileが残り得る。

MiniContainerは本番用のセキュリティー境界ではありません。
未信頼コードを扱うマルチテナント環境、guest escapeの防止、Linuxアプリケーション互換、ネットワーク隔離、永続ボリューム隔離、OCI互換、性能SLAは保証しない。
Windowsは対象外であり、Windows上の動作は検証しない。
