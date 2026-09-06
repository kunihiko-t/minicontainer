# 失敗、timeout、診断log

この章は、host error、guest failure、protocol破損を区別し、終了codeと標準エラー出力から原因を切り分ける方法を説明する。
run全体のlifecycleは第8章、CLIの構文と解決は第9章を参照する。

## 失敗の三分類

runの失敗は、出所で三つに分ける。
アプリケーションの非0終了は、ランタイムの失敗とは区別してゲストの結果として返す。
その値をアプリケーションの成功とみなすかは、呼び出し側が判断する。

- host error: bundle不正、期限の表現不能、payload pathの拒否、QEMU起動失敗、入出力error、cleanup失敗、QEMUの非0終了、出力合計の上限超過。
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
timeout、QEMU失敗、guest failure、protocol破損はすべて125に写る。

失敗の診断は`minictr:`で始まる行に出る。
まず標準エラー出力の先頭行を見る。

- `runtime process failed: process deadline elapsed`を含む行は全体のtimeoutである。
- `invalid UART control frame`を含む行はprotocol破損である。
- `guest reported an error`を含む行はguest自身の実行失敗である。
- `runtime session failed: QEMU exited unsuccessfully`を含む行はExit後のQEMU非0終了である。
- `guest output exceeds 1 MiB`を含む行は出力合計の上限超過である。
- `; cleanup also failed`を含む行は、主操作に加えて後始末も失敗したことを示す。

guest stderrのbytes自体にも`minictr:`は付かない。
ただしゲストも同じ接頭辞や終了コードを出力できるため、接頭辞だけで出所を確定できない。
型による区別が必要な呼び出し側は、CLIの文字列解析ではなくRust APIを使う。

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

## 出力合計の上限

stdout、stderr、diagnosticsの合計蓄積量は1 MiBが上限である。
frame単位の64 KiB制限とは別に、合計が上限を超えたframeを受け取った時点でrunを失敗させる。
Exit後の`Diagnostic`も合計に数えるため、終了確定後の連打でhost memoryが伸びることはない。
firmware由来のboot textは64 KiBの別上限で抑える。

上限超過はhost errorであり、終了code 125に写る。
ランタイムの失敗時には`RunOutcome`が返らないため、途中まで蓄積した出力は転送しない。
出力上限経路の検証では、125終了と診断の一致に加えてQEMUと一時領域の残留がないことを確認する。

## 診断logの扱い

`RunOutcome::diagnostics`は、firmware由来のboot textと`Diagnostic` frameのpayloadを連結したbytesである。
Exit後の成功markerもここに集まる。
成功失敗の判定には使わず、終了codeとerror分類で結果を判定する。
`minictr run`は現状この診断bytesを出力せず、stdout、stderr、終了codeだけを返す。

QEMU processの標準エラー出力は読み捨てにする。
ホスト標準エラー出力には、実行成功時のゲスト標準エラー出力と、CLIが生成する失敗診断が届く。
ランタイムの失敗時には`RunOutcome`が返らないため、途中まで蓄積したゲスト出力と診断は表示されない。
