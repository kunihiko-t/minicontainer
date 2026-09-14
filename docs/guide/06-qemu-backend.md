# QEMU backend

この章は、再現可能なcommand lineでQEMUを起動し、子processとして管理する方法を説明する。
payloadの用意は第5章、出力と終了の扱いは第7章と第8章を参照する。

## 決定的なcommand line

`QemuCommand::new`はkernel path、payload path、resource量から引数順まで固定の起動commandを組み立てる。
programは`qemu-system-riscv64`である。

```text
-machine virt -m <memory>M -smp <cpus> -bios default
-kernel <kernel>
-device loader,file=<payload>,addr=0x87800000,force-raw=on
-serial stdio -monitor none -display none
```

machineは`virt`である。memoryとvCPUはCLIで指定し、省略時は128 MiBと1 vCPUになる。
数値だけを書式化するため、QEMU引数への注入面はない。
serialはstdioへ直結し、monitorとdisplayは無効化するため、QEMUは対話なしで動く。
起動条件とkernel予約窓の検査はminiOS側が行い、host側はcommandの形だけを保証する。

## resourceの範囲

公開するmemoryは128から8192 MiB、vCPUは1から8である。
memoryの下限128は、payload予約窓`0x8780_0000..0x8800_0000`がRAMに載るために必要であり、既定値と一致する。
これより小さい値ではloaderがpayloadを配置できない。
上限8192 MiBと8 vCPUは学習用途の公開上限であり、CPU quotaやhost cgroup、hotplugは対象外である。

kernelはboot hartだけを使い、予約窓より上も管理しない。
追加のhartはOpenSBIに駐留したままguestへ影響せず、追加のmemoryも未使用のまま残る。
非既定値の起動はE2Eのresources経路 (`--memory 256 --cpus 2`で標準出力、標準エラー出力、終了code 42) で検証する。

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
