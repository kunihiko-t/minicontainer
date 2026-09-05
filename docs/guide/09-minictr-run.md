# `minictr run`を完成させる

この章は、imageの解決からQEMUの終了までを一つのCLI flowとして実行する方法を説明する。
store形式の詳細は第4章、runのlifecycleは第8章を参照する。

## 構文と解決

`minictr`が実装するcommandは`run`だけである。

```text
usage: minictr run [--store PATH] [--kernel PATH] [--timeout-ms N] IMAGE
```

`--store`と`--kernel`の値はOS pathとして不透明に扱う。
`--`で始まり`=`を含む値も分割しない。
同じoptionの重複は、`--timeout-ms`を含めて型付きerrorになる。
`--timeout-ms`の既定値は5000であり、0と非数値は拒否する。
command名、option名、image名はUTF-8でなければならず、storeとkernelの値だけが非UTF-8 byteを透過的に扱う。

省略時の解決順は、明示option、環境変数`MINICTR_STORE`と`MINICTR_KERNEL`、既定pathである。
既定のstoreは`$HOME/.minicontainer`、既定のkernelはその下の`minios-kernel`である。
`HOME`がなく既定pathを作れない場合は型付きerrorになる。

## 実行と入出力

解決したimage tagはcontent-addressed storeから検証済みbundle bytesとして取り出す。
bundleとkernel pathと期限をruntimeへ渡し、guest outcomeを受け取る。
guest stdoutはhost標準出力へ、guest stderrはhost標準エラー出力へ、そのまま書き出す。
途中で書き込みに失敗したらhost失敗として終わる。

## 終了code

guestの終了codeは0から255の範囲でそのままprocess終了codeになる。
範囲外の終了codeはhost失敗として扱う。
使い方の誤りは終了code 2、store解決失敗とruntime失敗は終了code 125である。
timeout、QEMU失敗、guest failure、protocol破損はすべて125に写り、診断は標準エラー出力へ出る。

## 失敗の調べ方

まず標準エラー出力の先頭行を見る。
`minictr:`で始まる行がhost側の分類 (store、process、session、cleanup) を示す。
`invalid UART control frame`を含む行はcontrol protocolの破損である。
`guest reported an error`を含む行はguest自身の実行失敗である。
`deadline elapsed`を含む行は全体のtimeoutである。
成功markerの有無ではなく、終了codeとこの分類で結果を判定する。
