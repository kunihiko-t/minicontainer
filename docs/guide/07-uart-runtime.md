# UART frameと標準入出力

この章は、分割されたcontrol frameをdecodeし、stdout、stderr、終了通知、診断を区別する方法を説明する。
QEMUの起動と読み取りは第6章、run全体の確定は第8章を参照する。

## Readyまでのboot text

QEMUのserialには、control streamの前にfirmware由来のtextが流れることがある。
`Session`は先頭のmagic `MCF1`を見つけるまでをboot診断として蓄積し、64 KiBを超えたら拒否する。
magicの途中まで一致した末尾は次回入力とつなげて判定するため、chunk境界でReadyを見失わない。

## Frameの種別

Readyの後はUARTがcontrol frameだけになる。
decoderはheaderとpayloadを復元し、不正なheaderと64 KiBを超えるpayloadを拒否する。
`Session`の状態は`AwaitReady`、`Running`、`Exited`の三つである。

- `Ready`はABI versionのhandshakeである。期待と異なるversionは拒否する。
- `Stdout`と`Stderr`はguest出力を連結して`RunOutcome`へ集める。`run_with_sink`では同じchunkを終了前に転送先へ逐次渡す。
- `Diagnostic`はguestの診断bytesを連結する。成功失敗の判定には使わない。
- `Exit`はu32 little-endianの終了codeを確定する。
- `GuestError`はguest自身の実行失敗であり、型付きerrorとしてrunを失敗させる。

Readyより前の出力frame、二度目のReadyは拒否する。

## Exit後のwire契約

固定revisionのminiOS kernelは、Exit frameの後にresource回収を検証する。
回収成功のmarkerは`println!`が`Diagnostic` frameになったものであり、hostの診断として蓄積する。
確定した終了codeは変わらない。
回収失敗やfatal trapでは`emergency_print`が`GuestError` frameになり、runは型付き失敗になる。

Exitの後に出力、再handshake、再終了のframeが届いた場合は拒否する。
出力が確定結果を書き換えることはない。

## 終了の確定

QEMU終了時に`Session::finish`がrunを確定する。
Exit未受信のまま終われば、QEMUの終了codeにかかわらず失敗になる。
decoderに未完のframeが残っていても失敗になる。
Exit受信後にQEMUが非0終了した場合も失敗になる。
正常なrunだけが分離したstdout、stderr、終了code、診断を返す。
