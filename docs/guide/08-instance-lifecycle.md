# Instance状態と後始末

この章は、一つのrunの起動から終了までの状態遷移、timeout、一時領域のcleanupを説明する。
QEMU起動の詳細は第6章、frameの意味は第7章を参照する。

## 副作用の前の検査

`Runtime::run`は最初に待機期限の表現可能性を検査する。
`Instant`に加算できない期限は、payloadも子processも作らず型付きerrorで返す。
次にMiniBundleを検証し、不正なbundleはQEMU起動前に拒否する。

## event loop

起動後は子processのeventを一つずつ処理する。
`Uart`はsessionへ送り、`Diagnostic`はQEMU由来の診断として読み捨てる。
`Exited`はsessionの確定へ進み、`TimedOut`と読み取りerrorはrun失敗へ進む。
session error (protocol破損、guest failure、Exit欠落を含む) もrun失敗へ進む。

loopの先頭では毎回時計を見る。
backendが期限過ぎの出力を返し続けても、全体の期限でrunはtimeoutする。
この検査と`next_event`側の検査の二層で、出力量にかかわらず期限が効く。

## 逐次転送

`Runtime::run_with_sink`は、decode済み`SessionEvent`を`sink`へguest frameの順序どおり渡しながらrunする。
stdoutとstderrのchunkは区別を保ったまま終了前に届くため、長い処理の進行を観測できる。
転送しても`Session`の蓄積は変わらないため、1 MiB上限は表示済みbyteを含めた合計で効く。

`sink.push`は同期呼び出しであり、遅いconsumerはevent loopへbackpressureをかける。
ただし全体の期限は延びない。停滞から復帰したloopは先頭の時計検査でtimeoutし、通常の後始末でQEMUを回収する。
consumerが`Err`を返したらrunは`RuntimeError::Consumer`で中断するが、QEMUの回収とpayload削除は行う。
`Runtime::run`は転送しない旧来の振る舞いであり、出力は`RunOutcome`にだけ集まる。

## 終了とcleanup

主結果が決まったら、`terminate_and_reap`を呼び、子プロセスの停止と回収を試みる。
終了済みの子はそのstatusを回収し、動作中の子はkillしてから回収する。
次にpayload fileとdirectoryの削除を試みる。

主操作と後始末の失敗は両方残す。
主操作だけ失敗すればそのerror、後始末だけ失敗すればcleanup error、両方失敗すれば主errorにcleanup診断を添えて返す。
主操作が成功して後始末が失敗した場合も、成功として扱わずcleanup errorを返す。

## host signalとgrace

QEMUは`minictr`とは別のprocess groupのleaderとして起動する。
端末のCtrl-Cや`minictr`のgroup宛signalはhostだけへ届くため、QEMUへsignalを届ける経路はhost側の転送だけである。

`minictr`はSIGINTとSIGTERMを捕捉し、最初のsignalをQEMUのprocess group全体へ転送してから2秒のgraceを始める。
grace中もguest出力のdecodeと転送は続き、grace内にQEMUが終了すれば通常の後始末へ進む。
graceを過ぎてもQEMUが残る場合はgroup全体へSIGKILLを送って回収し、grace中の2回目以降のsignalはgraceを待たずにSIGKILLへ進める。

guestのExit frame受信後に届いたsignalは結果を変えず、確定済みのguest結果を返す。
Exit前の中断は`RuntimeError::Interrupted`として報告し、転送や強制kill、後始末の失敗は中断errorへcleanup診断を添えて返す。
`InterruptSource`を渡さないembedderのrunはsignalを観測せず、hostがsignalで死んだ場合はchildをreapできない。

## instance状態の記録

`RunRequest.instances`があれば、runtimeはQEMU childのspawn直後にstoreの`run/`へstate fileを作る。
記録できないinstanceを起動したままにはしないため、登録に失敗したrunはchildを回収してpayloadを消してから`RuntimeError::Instance`で終わる。
state fileにはQEMUのpid、process開始token、comm名、image label、作成時刻を記録する。

主結果が決まった後のcleanupでは、QEMU回収とpayload削除ののちにstate fileを消す。
削除は冪等であり、失敗はcleanup errorへ合成される。
hostが`SIGKILL`や電源喪失で即死した場合だけstate fileが残り、`minictr ps`がpid identityを照合してliveまたはstaleとして表示する。
file形式と照合の契約は[instance state](../reference/instance-state.md)を参照する。

## 失敗の区別

呼び出し側は次の失敗を区別できる。

- bundle不正: QEMU起動前の拒否。
- QEMU起動失敗: spawn error。
- timeout: 全体の期限切れ。
- 出力上限超過: stdout、stderr、diagnosticsの合計が1 MiBを超えた場合のhost拒否。表示済みbyteも合計に含める。
- consumer失敗: 逐次転送先の書き出し失敗。QEMU回収とpayload削除は行う。
- instance登録失敗: state fileを書けないrun。QEMUは起動直後に畳まれる。
- guest failure: `GuestError` frame。
- protocol破損: 不正header、truncated frame、payload上限超過。
- applicationの非0終了: guestの終了codeをそのまま返す。
- host失敗: QEMU非0終了、入出力error、cleanup失敗。

`minictr`は0〜255のゲスト終了コードをプロセス終了コードへ写し、範囲外やホスト側の失敗を終了コード125へ写す。
実行中のSIGINT (Ctrl-Cを含む) とSIGTERMはQEMUのprocess groupへ転送したうえで中断として扱い、通常の後始末を経て125で終わる。
SIGKILLは捕捉できないため、後始末を保証せず子processや一時fileが残る場合がある。
詳しいCLIの振る舞いは第9章を参照する。
