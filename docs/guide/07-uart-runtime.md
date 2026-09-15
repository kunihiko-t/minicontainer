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

- `Ready`はABI versionのhandshakeである。majorは完全一致を要求し、guestのminorはhostのpin留めminor以下だけを受理する。
- `Stdout`と`Stderr`はguest出力を連結して`RunOutcome`へ集める。`run_with_sink`では同じchunkを終了前に転送先へ逐次渡す。
- `Diagnostic`はguestの診断bytesを連結する。成功失敗の判定には使わない。
- `Exit`はu32 little-endianの終了codeを確定する。
- `GuestError`はguest自身の実行失敗であり、型付きerrorとしてrunを失敗させる。

Readyより前の出力frame、二度目のReadyは拒否する。

## hostからguestへのstdin

Guest ABI v1.1以降 (minor 1以上) では、hostからguestへ`Stdin` frameを送れる。
`RunRequest::input`にreaderを渡したrunは、`Ready`のhandshake完了後にforwarder threadを起こし、入力を最大4 KiBのchunkへ分けて`Stdin` frameとしてQEMUのstdin pipeへ書く。
4 KiBはkernelのstdin stagingと同じ大きさであり、一つのframeが一度のguest `read`へ届く上限である。
readerがEOFに達すると、長さ0の`Stdin` frame (EOF sentinel) を送って閉じる。guestの`read`は以後0を返し続ける。

`input`が`None`のrunは`STDIN` frameを一切送らず、guestの`read`はframe到着まで待ち続ける。
guest ABI v1.0 (minor 0) は`STDIN` frameを持たないため、入力つきのrunは`RuntimeError::InputAbiUnsupported`で拒否され、frameは一つも送信されない。

backpressureはQEMUのchardev bufferが担う。
guestのUART受信が追いつかないときはQEMUがpipeの読み取りを止め、forwarder threadの`write`がblockする。
host側に入力bufferや上限counterは持たないため、入力のbyte数自体に上限はなく、大きな入力でもOOMしない。
guestが先に終了するとQEMUはstdinを閉じ、forwarderは`BrokenPipe`で静かに終わる。
host stdinのreadまたはQEMU stdinへのwriteが他の理由で失敗した場合は`RuntimeError::Input`でrunを失敗させ、通常のcleanup (QEMU kill、payload削除) を通す。

forwarder threadはrunの終了を待たずdetachされる。
guest終了後にhost stdinがopenのまま入力を待つ場合、threadはprocess終了または次の入力byteまでreadの中に残ることがある。
`minictr`ではprocess終了とともに回収されるが、runtimeをlibraryとして長命processへ組み込む場合は、run完了後もstdin readerをopenにしないことが要る。

single-imageのv1 bundleを実行するruntimeにとって、UART出力から届く`Stdin` (host→guest専用) と`ProcExit` (multi-image bundle専用) はどちらも契約外である。
受信した場合は`SessionError::UnexpectedKind`で拒否する。

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
