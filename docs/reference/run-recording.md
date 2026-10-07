# 監視と永続結果の内部基盤

`minicontainer-runtime::RunRecords`は、呼び出しprocessがQEMUを所有して監視するopt-in APIである。
新しい公開CLI command、daemon登録、v2 detachedはまだ提供しない。
既存のforeground、v1 detached、state v1、`ps`と`stop`の意味を変更しない。
Rust crate API自体は[公開互換性保証](compatibility.md)の対象外である。

## 保存先と観測

呼び出し側が指定したstoreの`results/r-<pid>-<開始token>-<sequence>/`へ保存する。
directoryはowner専用0700、`snapshot`と`stdout.log`・`stderr.log`は0600とする。
既存directoryの権限を変更して流用せず、別owner・公開permission・symlinkを拒否する。
共有logには秘密情報が入り得る。自動送信・公開・標準端末への転送はしない。
logはdecode済みguest stdout/stderrであり、firmware診断やtask別streamではない。
合計1 MiBまで保存し、読取にも上限とregular file検査を適用する。

実行前に未確定snapshotを保存する。監視processのPID・開始token・commとboot identityを記録し、
同じ監視役が生きていれば`Running`、監視役喪失・identity不一致・別boot・handle放棄なら`Unknown`となる。
Runningは監視identityの一致だけを示し、guestの進捗や成功は保証しない。
壊れたsnapshot、未知version、欠落fileは読取errorとなり、成功には変換しない。
古いinstance stateはこのAPIへ移行せず、従来の`ps`と`stop`が扱う。

`RecordedRun::supervise`は既存runtimeの全UART、QEMU終了status、reap、payloadとstateのcleanupまで待つ。
全てを終えてからlogをsyncし、version付きsnapshotを一時file・sync・rename・directory syncで確定する。
新規directoryの親もsyncする。未公開の部分書込fileは確定結果として読まない。
guest終了は`Guest`、timeout・protocol・QEMU・cleanupの失敗は`HostFailure`として区別する。
最終保存に失敗した場合は、保存errorとruntimeの主結果を両方返す。
監視役喪失や電源断で確定記録がない場合、UARTに終了通知があっても成功を推測しない。

共有logと確定結果はQEMU/payloadの回収後も残す。結果の自動削除・保持期間・公開CLIのwait規則は次段で定める。
既存`stop`は`run/`とpayloadを扱い、この独立した`results/`を削除しない。
stopとruntime cleanupが競合した場合も、cleanup errorを隠さずHostFailureとして保存する。
監視process自身の強制終了時に孤児QEMUを自動回収する保証はまだない。既存state v1と`stop`で回収する。

## sourceから試す

bundleを既存CLIでexportした後、次のsource exampleで正常終了・出力保存を確認できる。
この例はforegroundで監視し、stdinにはEOFを送り、guestの終了codeまたはhost失敗125で終了する。
既存detachedのstdinやtimeout契約を変えるものではない。

```sh
cargo run -p minicontainer-runtime --example record-run --locked -- "$STORE" "$BUNDLE" "$KERNEL" 5000
```

stdoutの`record=`行が保存名であり、共有logは上記の明示保存先に残る。
通常の`cargo xtask check`は実QEMUで正常42、timeout、既存stopとの競合、確定結果の再読取、共有log一致とcleanupを検査する。
host検査には監視役終了、別boot、identity不一致、異なるownerのhandle、部分snapshot、保存失敗、権限とsymlinkを含む。
次段はこの基盤を使ったdetached監視の起動・再接続と観測CLI、それからv2 detachedである。
