# MiniBundleのmanifestとdigest

この章は、canonical bundleを構築し、layout、入力上限、SHA-256 digestを検証できるようにする。
boot payloadへの配置は第5章、storeからの解決は第9章を参照する。

## layout

MiniBundle v1は、96 byte header、UTF-8 manifest、zero padding、静的RISC-V 64 ELFを一つのfileへ格納する。
headerはmagic `MINICTR\0`、ABI version 1.0、全体長、manifest範囲、ELF範囲、SHA-256 digestを持つ。
manifestはheader直後のoffset 96から始まり、ELFは8 byte境界に揃える。
bundle全体の上限は8 MiBである。

`build`は同じ入力から同じbytesを作る。
`parse`はheader、申告長と実長の一致、digest、paddingのzero、manifestを順に検証する。
ELFの中身はhostでは解釈せず、guestのloaderが検証する。

## manifestの文法と上限

manifestはUTF-8 textであり、全体で4 KiB以内、末尾にLFを一つ付ける。
一行目は`version=1`、二行目は`name=`、以降は`arg=`だけを置く。
順序違い、重複するversion、未知のkeyは拒否する。

nameは空でなく128 byte以内であり、ASCII英数字と`.`、`_`、`-`だけを使う。
argは16個まで、一つ256 byte以内であり、NULとCRを含めない。
LFは行区切りになるため、nameとargの値には使えない。

## digestとstore

digestは、digest欄をzeroにしたheaderと後続bytes全体へのSHA-256である。
header、manifest、padding、ELFのどこか一つの破損もdigest不一致として検出する。
digestは破損検出とcontent addressingに使い、署名や配布元の認証を提供しない。

ストアのルートは絶対パスで指定する。
ルートそのものと管理するディレクトリー、読み取るエントリーのシンボリックリンクを検査するが、親ディレクトリーのリンクまで一律に拒否するわけではない。
ストアを同時に敵対的なプロセスが変更する状況の封じ込めは保証しない。
`import`は検証済みbundleをdigest名で`images/sha256`へ書き込み、`tag`はtag名と小文字hex digestを対応付ける。
`resolve`はtagからdigestを引き、bytesを再検証してdigestとpathの一致を確認してから返す。
tag名はmanifestのname文法に従い、単一path成分でない名前は別途拒否する。

bundle全体の上限8 MiBは`minicontainer_bundle::MAX_BUNDLE_LEN`として公開し、`minictr image build`のELF読み取りも同じ上限を使う。
digestの小文字hex64桁への変換は`minicontainer_bundle::format_digest`として公開し、store path、tag、CLIの成功表示で共有する。
CLIの表示では先頭に`sha256:`を付ける。
