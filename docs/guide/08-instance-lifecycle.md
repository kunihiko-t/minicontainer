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

## 終了とcleanup

主結果が決まったら、`terminate_and_reap`を呼び、子プロセスの停止と回収を試みる。
終了済みの子はそのstatusを回収し、動作中の子はkillしてから回収する。
次にpayload fileとdirectoryの削除を試みる。

主操作と後始末の失敗は両方残す。
主操作だけ失敗すればそのerror、後始末だけ失敗すればcleanup error、両方失敗すれば主errorにcleanup診断を添えて返す。
主操作が成功して後始末が失敗した場合も、成功として扱わずcleanup errorを返す。

## 失敗の区別

呼び出し側は次の失敗を区別できる。

- bundle不正: QEMU起動前の拒否。
- QEMU起動失敗: spawn error。
- timeout: 全体の期限切れ。
- 出力上限超過: stdout、stderr、diagnosticsの合計が1 MiBを超えた場合のhost拒否。
- guest failure: `GuestError` frame。
- protocol破損: 不正header、truncated frame、payload上限超過。
- applicationの非0終了: guestの終了codeをそのまま返す。
- host失敗: QEMU非0終了、入出力error、cleanup失敗。

`minictr`は0〜255のゲスト終了コードをプロセス終了コードへ写し、範囲外やホスト側の失敗を終了コード125へ写す。
実行中のCtrl-Cは`minictr`自身がSIGINTを無視するため、QEMUだけが終了してExit欠落のhost失敗になり、通常の後始末を経て125で終わる。
詳しいCLIの振る舞いは第9章を参照する。
