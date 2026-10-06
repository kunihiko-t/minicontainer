# 複数taskのforeground実行

CLIと候補kernelのビルドから正常2task・fault分離まで実行する手順は
[複数taskのサンプル](../../examples/multi-task/README.md)を参照する。

manifest v2のbundleを`minictr image import`で登録し、通常の`minictr run`で実行できる。
内部builder `minicontainer_bundle::build_multi`は1〜4 imageを入力順で連結し、
ELF領域先頭からの相対offsetを生成する。import時にもELF範囲、重なり、manifestとbundle上限を検証する。
既存のv1 builder、wire ABI 1.2、ABI crate 0.2.0、リリースkernel固定は維持する。

初期imageのPIDは0からimage数未満であり、全PIDの`ProcExit`を必須とする。
実行中にspawnされたPIDの結果も回収し、重複PID、初期PIDの欠落、v1 `Exit`の混入はhost errorになる。
結果はPID順に保持し、CLIの終了codeはPID順で最初の非zero codeを選ぶ。全結果がzeroなら0になる。
`ProcExit`は実行全体の完了ではなく、hostはQEMU終了までUARTを読み続ける。
末尾の`GuestError`やQEMU失敗、後片付け失敗は結果を成功扱いせず、CLIはhost error 125を返す。
ゲスト終了code自体は既存のCLI範囲検査に従う。

各結果は到着時にstderrへ`minictr: process pid=N code=C`と表示する。
この診断文言は安定した公開契約ではない。stdout/stderrは共有streamであり、wireにPIDがないためtask別出力には分離しない。
出力上限1 MiBにはPID結果のwire payload 8 bytesずつも含め、結果数による無制限な蓄積を防ぐ。
stdin/EOFとdeadline、signal、process・一時fileの回収は既存foreground経路を使う。

detached実行は起動前に拒否する。`image build-multi TAG --image NAME ELF ...`で複数imageを構築できる。
引数の所属と入力規則は[CLI手順](../guide/09-minictr-run.md#複数imageのbundleを構築する)を参照する。
`image inspect`のname/argsとOCI configは先頭imageの情報を使い、bundle layer自体は全imageのbytesを保持する。
OCI configから複数taskの構造を復元する契約は設けない。

## 選択したMiniOSでの検証

```sh
cargo xtask compat --minios-rev 086f2e3fa54f751cd28c5fcd2bed205830bd2934 --multi
```

SHAは小文字40桁で明示する。`--multi`はtask-local fault処理を含むMiniOS revision
`805e6cc1c44da516d6a8b66028aa328893341a21`以降を要求する。
検証対象checkoutはcleanかつ指定HEADでなければならず、候補用cacheとCargo target dirはリリース経路から分離する。
通常のv1 E2E（READY、stdin/EOF、stdout/stderr、exit 42、timeout、cleanup）を先に実行し、
公開build-multi/exportのbytes一致と正常2 taskのPID別code 7を確認し、
その後illegal instructionとstore faultのcode 70、survivorの出力とcode 7、最終code 70、cleanupを確認する。
Ubuntu CIは上記の検証SHAを明示して継続検査する。候補の更新はリリースkernel更新とは独立である。
物理FPGA、macOS実機でのQEMU動作、task別I/Oはこの検査の対象外である。
