# 失敗、timeout、診断log

この章は、host error、guest failure、protocol破損を区別し、終了codeと標準エラー出力から原因を切り分ける方法を説明する。
run全体のlifecycleは第8章、CLIの構文と解決は第9章を参照する。

## 失敗の三分類

runの失敗は、出所で三つに分ける。
applicationの非0終了は失敗ではなく、guestの結果としてそのまま返す。

- host error: bundle不正、期限の表現不能、payload pathの拒否、QEMU起動失敗、入出力error、cleanup失敗、QEMUの非0終了。
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
- `; cleanup also failed`を含む行は、主操作に加えて後始末も失敗したことを示す。

guest stderrのbytes自体にも`minictr:`は付かない。
prefixの有無で、guest出力とhost診断を見分ける。

## timeoutの二層強制

全体の期限は`--timeout-ms`で与え、既定値は5000である。
event loopの先頭で毎回時計を見て、期限を過ぎたらtimeoutで終わらせる。
`next_event`側の待機にも同じ期限を渡す。
この二層で、出力量にかかわらず期限が効く。

timeout後も子processは自動で止まらない。
呼び出し側が`terminate_and_reap`で止めて回収し、payloadを削除する。
timeout経路の検証では、125終了に加えてQEMUと一時領域の残留がないことを確認する。

## 診断logの扱い

`RunOutcome::diagnostics`は、firmware由来のboot textと`Diagnostic` frameのpayloadを連結したbytesである。
Exit後の成功markerもここに集まる。
成功失敗の判定には使わず、終了codeとerror分類で結果を判定する。
`minictr run`は現状この診断bytesを出力せず、stdout、stderr、終了codeだけを返す。

QEMU processの標準エラー出力は読み捨てにする。
host標準エラー出力へ届くのは、UART上の`Stderr` frameとして届いたguest stderrだけである。
