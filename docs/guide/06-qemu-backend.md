# QEMU backend

この章は、再現可能なcommand lineでQEMUを起動し、子processとして管理する方法を説明する。
payloadの用意は第5章、出力と終了の扱いは第7章と第8章を参照する。

## 決定的なcommand line

`QemuCommand::new`はkernel pathとpayload pathから引数順まで固定の起動commandを組み立てる。
programは`qemu-system-riscv64`である。

```text
-machine virt -m 128M -smp 1 -bios default
-kernel <kernel>
-device loader,file=<payload>,addr=0x87800000,force-raw=on
-serial stdio -monitor none -display none
```

machineは`virt`、memoryは128 MiB、hartは一つである。
serialはstdioへ直結し、monitorとdisplayは無効化するため、QEMUは対話なしで動く。
起動条件とkernel予約窓の検査はminiOS側が行い、host側はcommandの形だけを保証する。

## 子processの起動と読み取り

`SystemProcessBackend`はstdoutとstderrをpipeで受けて子を起動する。
二つのreader threadが両pipeを並行してdrainし、届いたbyte列を`Uart`と`Diagnostic`のeventへ分けて送る。
終了は`try_wait`のpollで観測する。
読み取り専用threadが先にEOFを見ても、子processの終了回収は呼び出し側の`terminate_and_reap`が担う。

## 終了と回収

`terminate_and_reap`は、先に終了していればそのstatusを回収する。
動いていればkillしてから`wait`で回収する。
killと`try_wait`の競合で既に終了していた場合は、そのstatusを使う。
killできないのに終了もしていない場合だけ型付きerrorを返す。

`SystemProcess`の`Drop`は`terminate_and_reap`を呼ぶ最終手段である。
呼び出し側がエラー経路で子を放棄した場合も、終了と回収を試みる。
ただし`Drop`からは失敗を返せず、ホスト停止や`SIGKILL`では`Drop`自体が実行されない。
ただし公開の`ProcessBackend` traitがDrop実装を要求するわけではない。
自作backendでは呼び出し側が必ず`terminate_and_reap`を呼ぶ必要がある。

## 全体の期限

`next_event`はqueueにreader messageが残っていても、先に時計を見る。
期限を過ぎていれば残量にかかわらず`TimedOut`を返す。
出力が流れ続けてもtimeoutが先送りされないのは、この検査のおかげである。
詳しいlifecycleは第8章を参照する。
