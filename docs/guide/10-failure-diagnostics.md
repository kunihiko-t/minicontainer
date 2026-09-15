# 失敗、timeout、診断log

この章は、host error、guest failure、protocol破損を区別し、終了codeと標準エラー出力から原因を切り分ける方法を説明する。
run全体のlifecycleは第8章、CLIの構文と解決は第9章を参照する。

## 失敗の三分類

runの失敗は、出所で三つに分ける。
アプリケーションの非0終了は、ランタイムの失敗とは区別してゲストの結果として返す。
その値をアプリケーションの成功とみなすかは、呼び出し側が判断する。

- host error: bundle不正、期限の表現不能、payload pathの拒否、QEMU起動失敗、入出力error、cleanup失敗、QEMUの非0終了、出力合計の上限超過、consumer失敗。
- guest failure: `GuestError` frame。実行中の異常に加え、Exit後のresource回収失敗もここに入る。
- protocol破損: 不正header、truncated frame、順序違反、Exit欠落、ABI不一致、Exit payload違反、64 KiB超過（frame payloadとboot text）。

bundle不正と期限の表現不能は、payloadも子processも作らずに返す。
QEMU起動後の失敗では、必ず子processの回収とpayload削除を試みる。
主操作のerrorは残し、後始末でも失敗したら診断を添えて返す。
主操作が成功して後始末が失敗した場合も、成功として扱わない。

## 終了codeと標準エラー出力

`minictr`はguest終了codeを0から255の範囲でそのままprocess終了codeにする。
範囲外の終了codeはhost失敗として扱う。
使い方の誤りは終了code 2、store解決失敗とruntime失敗は終了code 125である。
timeout、QEMU失敗、guest failure、protocol破損、実行中のSIGINT/SIGTERMによる中断はすべて125に写る。
`doctor`の診断失敗は終了code 1であり、失敗項目は標準出力の`fail`行にすべて出る。

失敗の診断は`minictr:`で始まる行に出る。
まず標準エラー出力の先頭行を見る。

- `runtime process failed: process deadline elapsed`を含む行は全体のtimeoutである。
- `invalid UART control frame`を含む行はprotocol破損である。
- `guest reported an error`を含む行はguest自身の実行失敗である。
- `runtime session failed: QEMU exited unsuccessfully`を含む行はExit後のQEMU非0終了である。
- `run interrupted by SIGINT`または`run interrupted by SIGTERM`を含む行はhostが受け取ったsignalによる中断である。
- `instance state failed`を含む行はinstance stateの記録または削除の失敗である。storeの`run/`の権限とsymlinkを確認する。
- `QEMU exited before the guest was ready`を含む行はdetached runのhandshake失敗である。`qemu:`以降に`qemu.log`の末尾が続くことがあり、payload directoryとstate fileは回収済みである。
- `instance ... is not registered`を含む行は`stop`がstate fileを見つけられなかったことを示す。既に停止済みか、記録されていない。
- `instance ... state is corrupt`を含む行はidentity照合を信頼できないため、`stop`がprocessにもfileにも触れなかったことを示す。
- `instance ... was already gone`を含む行は`stop`の成功報告であり、標準エラー出力へ出るが終了codeは0である。
- `instance ... did not die after SIGKILL`を含む行はSIGKILL後もprocessが消えなかったことを示す。state fileとpayloadは残るため再試行できる。
- `instance was ... but cleanup failed`を含む行は停止自体は完了したが残骸の回収に失敗したことを示す。
- `guest output exceeds 1 MiB`を含む行は出力合計の上限超過である。
- `guest output consumer failed`を含む行は、run途中の標準出力・標準エラー出力への書き出し失敗である。QEMU回収とpayload削除は行われる。
- `; cleanup also failed`を含む行は、主操作に加えて後始末も失敗したことを示す。

guest stderrのbytes自体にも`minictr:`は付かない。
ただしゲストも同じ接頭辞や終了コードを出力できるため、接頭辞だけで出所を確定できない。
型による区別が必要な呼び出し側は、CLIの文字列解析ではなくRust APIを使う。

## doctorの失敗と対処

実行前に`doctor`で環境を確認し、`fail`行の`fix:`に従う。
診断は環境を変更しないため、対処の前後で何度でも実行できる。
`fail`行は標準出力に出るため、標準エラー出力ではなく標準出力を見る。

- `qemu: fail ... is not installed`はQEMUの未導入である。macOSでは`brew install qemu`、Ubuntuでは`sudo apt-get install qemu-system-misc`で導入する。
- `qemu: fail ... is too old`はQEMUが8.2.0未満である。行の更新手順で上げる。
- `qemu: fail ... failed with status`と`could not parse`はQEMUの導入破損である。QEMUを再導入する。
- `kernel: fail ... is missing`はkernel fileの不在である。固定revisionのminiOSをbuildして`--kernel`で渡す。
- `kernel: fail ... is not a file`と`is empty`はkernel pathの指定誤りである。正しいkernel fileを`--kernel`で渡す。
- `store: fail ... is missing`はstoreの未作成である。`image build`が作成するため、登録後に`doctor`を再実行する。
- `store: fail ... is not a directory`はstore pathの指定誤りである。directoryを`--store`で渡す。
- `cannot stat`はpath自体を読めない。権限とpathの綴りを確認する。

## distの失敗と対処

`cargo xtask dist`は入力の読み取り、archiveの組み立て、完成品の読み戻し検証の順に進み、失敗は終了code 1で標準エラー出力の一行に出る。

- `dist input minictr not found`と`dist input kernel not found`は入力fileの不在である。`--minictr`と`--kernel`のpathを確認する。
- `dist input license not found`はworkspace直下の`LICENSE-MIT`か`LICENSE-APACHE`の不在である。配布には両方が必須のため、削除せず復元する。
- `dist input ... is not a regular file`はdirectoryやdeviceの指定である。通常fileを渡す。
- `dist version is invalid`と`dist target is invalid`は`--version`と`--target`の綴りである。ASCIIの英数字と`.+_-`だけを使い、空や`.`や`..`を避ける。
- `dist archive verification failed`は完成品の自己検証の失敗である。`dist`内部の不具合を示すため、入力ではなく実装を疑う。
- `minictr version ... must match the xtask version`は公開gateのversion不一致である。`crates/minictr/Cargo.toml`と`xtask/Cargo.toml`のversionを揃える。

## timeoutの二層強制

全体の期限は`--timeout-ms`で与え、既定値は5000である。
event loopの先頭で毎回時計を見て、期限を過ぎたらtimeoutで終わらせる。
`next_event`側の待機にも同じ期限を渡す。
この二層で、出力量にかかわらず期限が効く。

`next_event`がタイムアウトを返すだけでは、子プロセスは停止しない。
`Runtime::run`が続けて`terminate_and_reap`を呼び、停止と回収、payloadの削除を試みる。
CLI利用者が通常のタイムアウト時に別途停止コマンドを実行する必要はない。
この期限はOSの入出力や終了後の後始末までを厳密に打ち切る時間制限ではない。
timeout経路の検証では、125終了に加えてQEMUと一時領域の残留がないことを確認する。

## signalによる中断

`minictr`はSIGINTとSIGTERMを捕捉し、最初のsignalをQEMUのprocess groupへ転送する。
QEMUは`minictr`とは別のprocess groupにいるため、端末のCtrl-Cや`minictr`宛のsignalはQEMUへ直接届かず、host転送だけが到達経路になる。
転送から2秒のgrace内にQEMUが終了しなければgroup全体へSIGKILLを送り、grace中の2回目以降のsignalは即座に強制回収する。
guestのExit受信後に届いたsignalは確定済みのguest結果を返し、Exit前の中断は`run interrupted by`診断と終了code 125で終わる。
中断経路の検証では、125終了と診断に加えてQEMUと一時領域の残留がないことを確認する。
`SIGKILL`は捕捉できないため、強制終了されたrunの後始末は保証しない。

強制終了で残ったinstanceはstoreの`run/`にstate fileが残り、孤児になったQEMUが生きていれば`minictr ps`でlive、死んでいればstaleと表示される。
`ps`は観測だけを行い削除しないため、残ったfileとQEMUは`stop`で回収する。
crash経路の検証では、`minictr`のSIGKILLとQEMUのkillのあと`ps`がstaleを表示することを確認する。

## detached runの診断

`run --detach`はsupervisorを残さないため、終了したguestを報告する仕組みはない。
guestのExit frameは誰も読まず、終了codeを得る経路はない。
`ps`が`live`を出し続けるだけではguestの生死は分からず、回収したい時点で`stop`を呼ぶ。

guestのUART出力とQEMU自身の診断はpayload directory内の`uart.log`と`qemu.log`へ書かれる。
どちらも合計量の上限がなく、`stop`でdirectoryごと消える。
handshake失敗時は`qemu.log`の末尾だけが`minictr:`行の`qemu:`以降へ出る。

## 出力合計の上限

stdout、stderr、diagnosticsの合計蓄積量は1 MiBが上限である。
frame単位の64 KiB制限とは別に、合計が上限を超えたframeを受け取った時点でrunを失敗させる。
Exit後の`Diagnostic`も合計に数えるため、終了確定後の連打でhost memoryが伸びることはない。
firmware由来のboot textは64 KiBの別上限で抑える。

上限超過はhost errorであり、終了code 125に写る。
ランタイムの失敗時には`RunOutcome`が返らないため、蓄積分が結果として返ることはない。
ただし`run_with_sink`では拒否前のchunkが既に転送先へ届いているため、上限超過の診断が出ても表示済みの出力は残る。
出力上限経路の検証では、125終了と診断の一致に加えてQEMUと一時領域の残留がないことを確認する。

## 診断logの扱い

`RunOutcome::diagnostics`は、firmware由来のboot textと`Diagnostic` frameのpayloadを連結したbytesである。
Exit後の成功markerもここに集まる。
成功失敗の判定には使わず、終了codeとerror分類で結果を判定する。
`minictr run`は現状この診断bytesを出力せず、stdout、stderr、終了codeだけを返す。

QEMU processの標準エラー出力は読み捨てにする。
ホスト標準エラー出力には、実行成功時のゲスト標準エラー出力と、CLIが生成する失敗診断が届く。
ランタイムの失敗時には`RunOutcome`が返らないため、途中まで蓄積したゲスト出力と診断は表示されない。
