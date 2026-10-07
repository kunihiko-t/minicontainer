# v1監視起動と結果への再接続

`minictr start` はv1 MiniBundleを監視役process経由で起動する。guestのREADYを確認してから、標準出力へ保存記録ID `r-<監視PID>-<開始token>-<連番>` を一行返す。起動commandの終了code 0はREADY確認を表し、guestの正常終了を表さない。入力はEOFとし、共有stdout/stderrはownerだけが読める保存logへ記録する。v2とtask別I/Oはまだ対応しない。

```sh
id=$(minictr start --store /path/to/store --kernel /path/to/minios-kernel --timeout-ms 10000 hello)
minictr status --store /path/to/store "$id"
minictr status --store /path/to/store --wait --timeout-ms 12000 "$id"
```

`minictr status` は別のterminalから同じstoreとIDで再接続できる。出力はheaderなしのtab区切り `STATE CODE INSTANCE` 一行。

| STATE | CODE | 意味 |
| --- | --- | --- |
| running | - | 同じidentityの監視役が生存し、結果は未確定 |
| exited | guest code | guest終了、QEMUの実status確認・wait/reap・payloadとstate回収、結果保存が完了 |
| failed | 125 | host/runtimeの失敗を確認し、結果保存を完了 |
| unknown | - | 監視役喪失・未完了記録・従来のstate v1。成功とは推測しない |

INSTANCE列は現boot・登録state・process開始token/commが起動時と一致する場合だけ、従来のQEMU instance ID `i-<pid>` を表示する。終了後、PID再利用、別runへのstate上書き、関連のない旧保存記録では `-`。保存IDから過去のPIDを停止対象として推測しない。

通常queryはrunning/exitedなら0、failed/unknownや読取失敗なら125で終了する。`--wait` は最大 `--timeout-ms`（省略時2秒）だけ結果を待ち、exitedならguest code（0〜255）を返す。表現できないguest codeは表示値を保ち125を返す。待機期限切れ・SIGINT/SIGTERMはqueryだけを中断し125を返し、実行は継続する。IDや引数の構文不正は2。

停止はINSTANCE列を使って従来の `minictr stop` を呼ぶ。保存結果とlogは削除しない。

```sh
minictr stop --store /path/to/store i-12345
```

従来の `minictr run --detach` とps/stopの振る舞いは変更しない。従来の `i-<pid>` に対するstatusはunknownであり、staleを終了code0へ変換しない。

監視役は通常のユーザーprocessで、サービス登録や追加権限を必要としない。起動前のsignalや期限切れは監視役へSIGTERMを送り、既存runtimeによる回収を待つ。SIGKILL・host再起動などで監視役が失われた場合、自動QEMU回収は保証しない。unknownと記録されたINSTANCE、または既存psを確認し、stopで残るstate/payloadを回収する。記録の破損・symlink・他owner・公開permissionは読取失敗として扱う。

内部の保存基盤は [run-recording.md](run-recording.md) を参照。`results/<id>/instance` は起動時の完全stateを保存するowner専用のQEMU関連sidecarで、既存snapshot形式は変更しない。共有logの合計上限は1 MiBで、今回log表示commandや自動削除機能は追加しない。
