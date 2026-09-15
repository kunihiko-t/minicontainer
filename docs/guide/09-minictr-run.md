# `minictr run`を完成させる

この章は、imageの登録と解決からQEMUの終了までを一つのCLI flowとして実行する方法を説明する。
store形式の詳細は第4章、runのlifecycleは第8章を参照する。

## 構文と解決

`minictr`が実装するcommandは`run`、`ps`、`doctor`、`image build`、`image import`、`image export`、`image list`、`image inspect`、`image remove`、`image prune`、`image export-oci`、`image import-oci`、`image pull-oci`、`help`、`--version`である。

```text
usage: minictr run [--store PATH] [--kernel PATH] [--timeout-ms N] [--memory MIB] [--cpus N] IMAGE
usage: minictr ps [--store PATH]
usage: minictr doctor [--store PATH] [--kernel PATH]
usage: minictr image build [--store PATH] [--arg VALUE]... IMAGE ELF
usage: minictr image import [--store PATH] IMAGE FILE
usage: minictr image export [--store PATH] IMAGE --output PATH
usage: minictr image list [--store PATH]
usage: minictr image inspect [--store PATH] IMAGE
usage: minictr image remove [--store PATH] IMAGE
usage: minictr image prune [--store PATH] [--dry-run] [--force]
usage: minictr image export-oci [--store PATH] IMAGE --output DIR
usage: minictr image import-oci [--store PATH] IMAGE DIR
usage: minictr image pull-oci [--store PATH] IMAGE REFERENCE
```

`help`と`--help`は上記のusageを標準出力へ出して0で終わる。
`--version`は`minictr 0.1.0`を標準出力へ出して0で終わる。
どちらも引数を取らず、後続のtokenは型付きerrorになる。

`--store`と`--kernel`の値はOS pathとして不透明に扱う。
`--`で始まり`=`を含む値も分割しない。
同じoptionの重複は、`--timeout-ms`を含めて型付きerrorになる。
`--timeout-ms`の既定値は5000であり、0と非数値は拒否する。
`--memory`はMiB単位の整数で、128から8192の範囲だけを受け付ける。
128はpayload予約窓がRAMに載る下限であり、既定値と一致する。
`--cpus`は1から8の整数だけを受け付ける。
どちらも省略時は128 MiBと1 vCPUのままである。
接尾辞 (`256M`など)、非数値、範囲外、u32に収まらない値は使い方の誤りになる。
同じoptionの重複も、`--timeout-ms`と同様に型付きerrorになる。
QEMUへ渡すのは検証済みの数値だけであり、引数注入の面はない。
resource範囲の根拠は[第6章](06-qemu-backend.md)を参照する。
command名、option名、image名はUTF-8でなければならず、storeとkernelの値だけが非UTF-8 byteを透過的に扱う。

`image build`の`--store`も一度だけ指定でき、重複は型付きerrorになる。
`--arg`は繰り返し指定でき、順にmanifestのゲスト引数になる。
IMAGEはUTF-8でなければならず、ELFは`--store`と同じくOS pathとして非UTF-8 byteを透過的に扱う。
`--arg`の値はmanifestへ格納するためUTF-8でなければならない。
optionはIMAGEとELFの前後どこに置いてもよい。
`image import`はIMAGEとFILEを取り、`--store`を前後どこに置いてもよい。
FILEは`--store`と同じくOS pathとして非UTF-8 byteを透過的に扱う。
`image export`はIMAGEを一つ取り、`--store`と必須の`--output`を前後どこに置いてもよい。
`--output`の値は`--store`と同じくOS pathとして非UTF-8 byteを透過的に扱う。
`image list`はpositionalを取らず、`--store`だけを一度だけ指定できる。
`ps`もpositionalを取らず、`--store`だけを一度だけ指定できる。
`image inspect`はIMAGEを一つ取り、`--store`を前後どこに置いてもよい。
`image remove`もIMAGEを一つ取り、`--store`を前後どこに置いてもよい。
`image prune`はpositionalを取らず、`--store`と値なしflagの`--dry-run`と`--force`だけを一度ずつ指定できる。
`--dry-run`と`--force`の併用と、flagへの`=値`の付与は型付きerrorになる。
`image export-oci`はIMAGEを一つ取り、`--output`は必須、`--store`は任意であり、optionは前後どこに置いてもよい。
`--output`のDIRは`--store`と同じくOS pathとして非UTF-8 byteを透過的に扱う。
`image import-oci`はIMAGEとDIRの二つを取り、`--store`は任意であり、optionは前後どこに置いてもよい。
DIRもOS pathとして非UTF-8 byteを透過的に扱う。
`image pull-oci`はIMAGEとREFERENCEの二つを取り、`--store`は任意であり、optionは前後どこに置いてもよい。
`image`にsubcommandがない場合はcommand不足、未知のsubcommandは未知commandの型付きerrorになる。
`doctor`はpositionalを取らず、`--store`と`--kernel`だけを一度ずつ指定できる。

省略時の解決順は、明示option、環境変数`MINICTR_STORE`と`MINICTR_KERNEL`、既定pathである。
既定のstoreは`$HOME/.minicontainer`、既定のkernelはその下の`minios-kernel`である。
`HOME`がなく既定pathを作れない場合は型付きerrorになる。
`image build`、`image import`、`image export`、`image list`、`image inspect`、`image remove`、`image prune`、`image export-oci`、`image import-oci`、`image pull-oci`のstore解決も同じ順序を使う。
`ps`も同じ順序でstoreを解決する。
`doctor`も`run`と同じ順序でstoreとkernelを解決する。

## imageの登録

`image build`は、ELFのmetadata確認、上限付きread、MiniBundleの構築、`import`、`tag`を順に行う。
成功すると次のようにタグとdigestの一行だけを標準出力へ出す。

```text
myapp sha256:<64桁の小文字16進数>
```

8 MiBはbundle全体の上限であり、ELF単体で超える入力は本体を読む前に拒否する。
実際に収まる最大のELFはheader・manifest・padding分だけ小さい。
ELFの中身はhostでは検証せず、guestのloaderが検証する。
tag名とゲスト引数の文法はbundle構築時に検証し、不正な入力はhost側の失敗として終わる。
tag付けに失敗した後に未参照のdigestが残ることは許容する。
同じbytesのblobは再利用でき、削除やrollbackを加えるほうがstore操作を複雑にするためである。

## imageの取り込み

`image import`は、MiniBundle fileを上限付きで読み、検証してからstoreへ登録する。
検証はstore操作より先に行い、不正bundleではlayout directoryも作らない。
成功すると`build`と同じくタグとdigestの一行だけを標準出力へ出す。

```text
myapp sha256:<64桁の小文字16進数>
```

manifest内のnameは書き換えず、CLIのtagだけを付ける。
配布物のnameと手元のtagが異なる場合があり、`image inspect`の`tag`と`name`で区別する。
同じbytesの再importは同じdigestを報告し、blobはcontent addressingで再利用する。
既存tagへのimportはtagだけをatomicに付け替え、置き換え前のblobは残す。

## imageの取り出し

`image export`は、tagまたは`sha256:`付きdigestで解決したbundleを検証してから`--output`へ書き出す。
成功すると`build`と同じく入力とdigestの一行だけを標準出力へ出す。

```text
myapp sha256:<64桁の小文字16進数>
```

書き出すbytesはstoreのblobと同一であり、解決時にdigestを再検証する。
digest指定は`sha256:`接頭辞が必須であり、接頭辞のない64桁はtagとして扱う。
出力先にfileやdirectoryがある場合は上書きせず失敗する。強制上書きのoptionはなく、先に取り除いてから再実行する。
書き出しは一時fileとrenameで行い、途中失敗で部分fileや一時fileを残さない。

## imageの確認

`image list`は次のようにheaderとbyte順の一覧を出す。
空のstoreではheaderだけを出す。

```text
TAG	DIGEST
myapp	sha256:<64桁の小文字16進数>
```

`image inspect`は次の安定した5行を出す。
`digest`は解決したbundle headerの値であり、tag fileの内容ではない。

```text
tag: myapp
name: myapp
digest: sha256:<64桁の小文字16進数>
args: 0
elf-bytes: 4096
```

manifest引数の内容は表示せず、件数だけを表示する。
未参照のtagは一覧に出るが、同じtagのinspectは`resolve`経由で失敗する。

## instanceの確認

`run`はQEMU起動直後にstoreの`run/`へinstance stateを記録し、終了時に消す。
`ps`は残っているstate fileを読み、次のようにheaderと5列の一覧を出す。
instanceがなければheaderだけを出す。

```text
INSTANCE	PID	IMAGE	STATE	AGE
i-40231	40231	myapp	live	12s
```

`INSTANCE`は`i-<pid>`の形の公開名であり、`PID`は記録されたQEMU childのprocess ID、`IMAGE`はrunに使ったtag、`AGE`は記録からの経過時間を`s`、`m`、`h`、`d`へ畳んだ値である。
`STATE`は`live` (記録pidが同じprocessとして生存)、`stale` (pidの死亡または再利用)、`corrupt` (parse不能またはsymlinkのfile) のいずれかである。
corrupt行は識別子だけを出し、残りの列を`-`で埋める。
一覧はid順であり、観測だけを行い、staleやcorruptのfileを削除しない。
削除は次のrunが通常cleanupで行うか、利用者がfileを直接消す。
file形式とpid identityの照合は[instance state](../reference/instance-state.md)を参照する。

## 実行前の診断

`doctor`は、QEMU、kernel file、store rootの三つの検査を順に行う。
QEMUは`qemu-system-riscv64 --version`の先頭行からversionを読んで8.2.0以上を要求する。
kernelは解決したpathが空でない通常fileであることを、storeは存在するdirectoryであることをmetadataだけで確認する。
存在しないstoreは作らず、診断は環境を変更しない。

検査結果は次の安定した4行を標準出力へ出す。
`fail`行は原因と`fix:`の対処を同じ行に載せる。

```text
qemu: ok qemu-system-riscv64 8.2.2
kernel: ok /store/minios-kernel (12345 bytes)
store: ok /store
summary: passed 3/3 checks
```

非UTF-8のpathは置換文字で表示し、改行を含むpathはescapeして一行を保つ。
失敗行の読み方は第10章を参照する。

## imageの削除

`image remove`は、指定したtagだけを削除し、blobと他のtagを保持する。
成功すると次のようにtagと結果の一行だけを標準出力へ出す。

```text
myapp removed
```

存在しないtagは型付きerrorで失敗する。tag fileの内容は検証せず、壊れたtagも削除できる。
tag名の文法検査、symlinkと非fileの拒否、store外への到達検査を経てからunlinkするため、store外のpathへは到達しない。
blobの削除は対象外であり、未参照blobの掃除は`image prune`で行う。

## 未参照blobの掃除

`image prune`は、どのtagからも参照されないblobを検出する。
引数なしと`--dry-run`は候補の表示だけで削除せず、`--force`の指定時だけ削除する。
候補も削除結果も次の安定した`sha256:`行で標準出力へ出す。

```text
sha256:<64桁の小文字16進数>
```

削除は候補ごとに参照状態を再確認し、再参照されたblobは飛ばして標準エラー出力へ記録する。
store側も参照中blobの削除を拒否する。部分失敗はblobごとに報告し、一つでもあれば終了code 125で終わる。
blob名でない配置物は候補に含めず、symlink名のblobは削除せず失敗として報告する。
確認と削除の隙間は狭めるだけでなくせない。並行する攻撃者への対策ではない。

## imageの配布

`image export-oci`は、storeのbundle bytesをOCI Image Layoutのdirectoryへexportする。
成功すると次のようにtagとindex digestの一行だけを標準出力へ出す。

```text
myapp oci-layout sha256:<64桁の小文字16進数>
```

出力先には`oci-layout`、`index.json`、config、manifest、MiniBundle layerの5 fileを置く。
出力先がすでに存在する場合は上書きせず型付きerrorで終わる。
fileは一時directoryに書いてからrenameで公開し、完成品を読み戻して検証してから成功を報告する。
layer blobはstoreのbundle bytesと同一であり、tagはlayoutに引き継がない。
media type、正準JSON、digestの対応は[MiniBundleとOCI Image Layoutの対応](../reference/minibundle-oci-mapping.md)が正である。

`image import-oci`は、layout directoryを検証してMiniBundleを復元し、IMAGEのtagでstoreへ登録する。
成功すると次のようにtagとbundle digestの一行だけを標準出力へ出す。

```text
myapp sha256:<64桁の小文字16進数>
```

検証の順序はsize、digest、JSON parse、意味検証、MiniBundle parseであり、一つでも失敗したらstoreを変更しない。
layout外への参照とsymlinkは拒否し、未知のplatformとmedia typeは型付きerrorになる。
復元したbundleは正準形に組み立て直し、storeには正準bytesだけを置く。
tagはCLI引数から付け、layoutのannotationは引き継がない。

`image pull-oci`は、registryからdigest pinで匿名pullし、IMAGEのtagでstoreへ登録する。
参照の形は`host[:port]/repository@sha256:<64桁の小文字16進数>`であり、tag指定と認証情報は受け入れない。
成功すると`IMAGE sha256:<digest>`の一行だけを標準出力へ出す。
取得はmanifest、config、layerの順に行い、検証が通るまでstoreを変更しない。
HTTPSだけを使い、認証を求めるregistryは型付きerrorで報告する。
redirect、timeout、size上限の値は[第12章](12-oci-image-spec.md)を参照する。

## 実行と入出力

解決したimage tagはcontent-addressed storeから検証済みbundle bytesとして取り出す。
bundleとkernel pathと期限をruntimeへ渡し、guest outcomeを受け取る。
ゲスト出力はdecodeされ次第、stdout chunkは標準出力へ、stderr chunkは標準エラー出力へ、guest frameの順序どおり逐次書き出す。
pipe越しでも進行が見えるよう、chunkごとにflushする。
二つのストリームは区別して書き出すが、ホスト側の二出力間の到達順序は保証しない。
run途中の書き込みやフラッシュ失敗はconsumer失敗としてrunを中断し、QEMU回収とpayload削除を経てホスト側の失敗で終わる。
ランタイムがエラーを返した場合、中断前に書き出した分は残るが、残りの出力は届かない。
実行中のSIGINT (Ctrl-Cを含む) とSIGTERMは`minictr`が捕捉してQEMUのprocess groupへ転送し、2秒のgraceののち必要ならgroup全体へSIGKILLで回収する。
grace中の2回目以降のsignalは即座に強制回収へ進み、guestのExit受信後に届いたsignalは確定済みのguest結果を返す。
runはQEMU起動直後にinstance stateをstoreへ記録し、終了時のcleanupで消す。state directoryを開けないstoreではrunを始めない。
crashで残ったinstanceは`ps`で確認できる。
E2Eのhappy pathは同梱ゲストを`image build`、`image inspect`、`run`へ一続きで通し、このflow全体を公開CLIで検証する。
割り込み経路はgroup宛のSIGINTと`minictr`のPIDだけへのSIGTERMを別々に検査し、どちらも125終了とQEMU・一時領域の非残留を確認する。

## 終了code

guestの終了codeは0から255の範囲でそのままprocess終了codeになる。
範囲外の終了codeはhost失敗として扱う。
使い方の誤りは終了code 2、store解決失敗とruntime失敗は終了code 125である。
timeout、QEMU失敗、guest failure、protocol破損、実行中のSIGINT/SIGTERMによる中断はすべて125に写り、診断は標準エラー出力へ出る。
`image build`では、ELFの読み取り失敗、上限超過、bundle構築失敗、import失敗、tag失敗が終了code 125になり、成功表示は出さない。
`image build`のparse失敗とstore pathの既定値解決失敗は使い方の誤りとして終了code 2になる。
`image import`では、fileの読み取り失敗、上限超過、bundle検証失敗、import失敗、tag失敗が終了code 125になり、成功表示は出さない。
`image import`のparse失敗とstore pathの既定値解決失敗は終了code 2になる。
`image export`では、image解決失敗、digest検証失敗、出力先の存在、書き出し失敗が終了code 125になり、成功表示は出さない。
`image export`のparse失敗、`--output`の不足、store pathの既定値解決失敗は終了code 2になる。
`image list`と`image inspect`では、store失敗と出力失敗が終了code 125になり、parse失敗とstore pathの既定値解決失敗は終了code 2になる。
`ps`では、instance stateのopen・一覧失敗と出力失敗が終了code 125になり、parse失敗とstore pathの既定値解決失敗は終了code 2になる。
`doctor`は全検査の成功で終了code 0、一つでも失敗したら終了code 1になる。
`doctor`のparse失敗と既定値解決失敗は終了code 2、出力失敗は終了code 125になる。
`image remove`では、tag不在とstore失敗と出力失敗が終了code 125になり、parse失敗とstore pathの既定値解決失敗は終了code 2になる。
`image prune`では、候補検出失敗、再確認失敗、削除の部分失敗、出力失敗が終了code 125になり、parse失敗とstore pathの既定値解決失敗は終了code 2になる。
再参照によるskipは失敗ではない。

## 失敗の調べ方

実行前に`doctor`で環境を確認し、`fail`行の`fix:`に従う。
まず標準エラー出力の先頭行を見る。
`minictr:`で始まる行がhost側の分類 (store、process、session、cleanup) を示す。
`invalid UART control frame`を含む行はcontrol protocolの破損である。
`guest reported an error`を含む行はguest自身の実行失敗である。
`deadline elapsed`を含む行は全体のtimeoutである。
`run interrupted by`を含む行はhostが受け取ったSIGINTまたはSIGTERMによる中断であり、signal名を行に示す。
ゲスト自身も2や125を返せるため、終了コードだけでは使い方の誤りやホスト側の失敗と区別できない。
`minictr:`という文字列もゲストが出力できるので、接頭辞は調査の手掛かりとして使う。
プログラムから失敗を厳密に区別する場合は、Rust APIの`Result`とエラー型を使う。
