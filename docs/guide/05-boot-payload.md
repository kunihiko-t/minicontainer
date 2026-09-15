# Boot payloadをゲストへ渡す

この章は、MiniBundleからboot payloadを作り、QEMUがminiOSへ見せる予約メモリーへ配置するまでを説明する。
bundle形式の詳細は第4章、QEMU command lineの詳細は第6章を参照する。

## MiniBundleの配置

MiniBundle v1は96 byte header、UTF-8 manifest、zero padding、静的RISC-V 64 ELFを一つのfileへ格納する。
`Runtime::run`はQEMUを起動する前に`minicontainer_bundle::parse`でbundleを検証する。
digest不一致、layout不正、manifest違反のbundleはここで拒否され、QEMUは起動しない。

## 一時payload file

検証済みbundle bytesは`PayloadTemp::create`で一時fileへ書き出す。
配置先は`minicontainer-run-<process id>-<連番>/payload.mcb`という新規directoryである。
directoryは`create_dir`で作るため、既存entryの上書きやsymlinkの追随は起きない。
unixではdirectoryが`0700`、fileが`0600`で作られ、同じhostの他userから読めない。
fileは`create_new`で開くため、既存fileへの上書きも起きない。

書き込み中に失敗した場合は部分fileと空directoryの除去を試みる。
除去まで失敗した場合は書き込みerrorを主原因、除去失敗をcleanup診断として両方返す。
部分fileが存在しないためのNotFoundは失敗として数えない。

## 予約メモリーへの受け渡し

一時payloadのpathはQEMUの`-device loader,file=<path>,addr=<window>,force-raw=on`へ展開される。
`<window>`は`RAM末尾 - 2 MiBのFDT予約 - 6 MiBのbundle窓`であり、`-m`の値から導く。既定の128 MiBでは`0x87800000`になる。
miniOSは起動時にこの予約窓からELF byte sliceを取り出し、既存のloader経路へ渡す。
ホスト側はELF mappingやuser pointer検証を複製しない。
pathに`,`を含む場合はQEMUのoption構文を壊すため、起動前に型付きerrorで拒否する。
`-kernel`へ渡すkernel pathはplainなargv要素のため、この制限の外にある。

payload fileとdirectoryはrunの終了後に削除を試みる。
正常終了だけでなく、timeout、起動失敗、後始末失敗のどの経路でも除去を試み、失敗は診断として残す。
詳しい状態遷移は第8章を参照する。
