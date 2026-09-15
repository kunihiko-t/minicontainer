# Instance state

`minictr run`はQEMU起動直後にinstanceのstate fileを作り、終了時に消す。
`minictr ps`は残ったfileを読み、各instanceをlive、stale、corruptとして一覧する。

## 置き場所

state fileはstore root直下の`run/`に一つの`<id>.state` fileとして置く。
`<id>`は`i-<pid>`の形であり、`<pid>`はrunが起動したQEMU childのprocess IDである。
`--store`や`MINICTR_STORE`でstoreを切り替えると、instanceの一覧もそのstoreのものに切り替わる。

`run/`はowner専用 (Unixでは0700) で作られる。
store rootと`run/`がsymlinkまたはdirectory以外であれば拒否し、canonical pathがstore rootの内側に留まることだけを受理する。
symlink検査は誤操作と破損への防御であり、並行する攻撃者への対策ではない。

## file形式

state fileはversion行と5つの`key=value`行を持つテキストであり、行の順序は固定である。

```text
minicontainer-state-v1
pid=<QEMU childのprocess ID>
token=<記録時のprocess開始token>
comm=<記録時の実行file名>
image=<psのIMAGE列に出すtag>
started=<state作成時点のunix epoch秒>
```

書き込みは一時fileへの完全な書き込みと`sync`ののちrenameで公開する。
`ps`が部分書き込みのfileを読むことはない。
parseはversion行とfieldの順序・必須性への厳密な一致だけを受理し、上限 (1 KiB) 超過、未知version、順序の入れ替わり、余分な行はすべてcorruptとして扱う。
`image`は行形式を壊す文字 (空白、`=`、改行) と128 byte超過を拒否する。

## live / stale / corrupt

`ps`は各fileを次のように分類する。

- **live**: 記録pidが生存し、process開始tokenとcomm名が記録と一致する。pid単体では再利用後の別processと区別できないため、開始時刻由来のtokenと実行file名を照合する。
- **stale**: 記録pidが死亡したか、tokenかcommが一致しない別processに再利用された。
- **corrupt**: 上限超過、不正bytes、version不一致、symlinkのfile。識別子だけを表示し、残りの列は欠損として出す。

macOSでは`proc_pidinfo`の`PROC_PIDTBSDINFO`から`pbi_start_tv{sec,usec}`と`pbi_comm`を取り、Linuxでは`/proc/<pid>/stat`の`comm`と`starttime`を取る。
process開始時刻を取得できないplatformではpid再利用を識別できないため、登録もlive判定も行わない。

## 登録と削除

`Runtime`は`RunRequest.instances`があれば、QEMU childのspawn直後に`register`を呼ぶ。
記録できないinstanceを起動したままにはしないため、登録失敗時はchildを回収してpayloadを消してから型付きerrorを返す。
通常終了・timeout・signal中断・失敗の全経路で、cleanupの最後に`unregister`がstate fileを消す。
`unregister`は冪等であり、既に無いfileの削除は成功として扱う。
削除の失敗はcleanup errorへ合成され、主errorを隠さない。

通常経路ではfileは残らない。
hostが`SIGKILL`や電源喪失で即死した場合だけ、QEMUの孤児とstate fileが残る。
`ps`はこれらを観測するだけであり、削除は行わない。
孤児QEMUがまだ動いていればliveとして表示され、死んでいればstaleとして表示される。
