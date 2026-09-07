# 静的ELFとアプリケーションABI

この章は、guestが受け入れるELFとGuest ABIの条件を説明できるようにする。
bundleへの格納は第4章、boot時の受け渡しは第5章を参照する。

## guestが受け入れるELF

guest applicationは、静的にlinkしたRISC-V 64 ELFである。
実行時はminiOSのELF loaderが読み込み、U-modeで実行する。
ELFのmappingと検証、user pointerの検査はminiOSが担当し、ホスト側へ複製しない。
Linux application互換は提供しない。

## Guest ABIの固定

MiniContainerはtagで固定したminios-abi (`minios-abi-v0.1.1`) を利用する。
boot header、manifest、UART control frame、system call番号をhostとguestで共有する。
実行kernelはminiOSの固定revisionからbuildする。
ABIを更新する場合は、タグとカーネルリビジョンの対応を確認し、ホストとゲストを組み合わせた検証を行う。

## system callの条件

Guest ABIが定義するsystem callは二つである。
`Write`は番号1、`Exit`は番号2である。
file descriptorは標準出力が1、標準エラー出力が2であり、一回の`write`上限は4096 byteである。
`write`の結果は`Stdout`と`Stderr`のcontrol frame、`exit`の結果は`Exit` frameとしてUARTへ届く。
frameの復元は第7章、run全体の確定は第8章を参照する。

## 同梱ゲスト例の読み方

[guest-hello](../../examples/guest-hello/README.md)は、この章の条件を満たす最小の`no_std`ゲストである。
`src/main.rs`は`_start`から`write`と`exit`だけを呼び、標準出力へ`hello from guest`、標準エラー出力へ`guest stderr`と書いて42で終わる。
system call番号とfile descriptorは`minios_abi::syscall`の定義だけを参照し、source内に番号を直書きしない。
`write`が要求byte数と違う結果を返した場合とpanic時は、42ではなく失敗code 1で終わる。

生の呼び出し規約は、番号を`a7`、引数を`a0`から`a2`、戻り値を`a0`で渡す。
`write`は順にfile descriptor、buffer address、byte数を受け取り、書けたbyte数か負のerrorを返す。
`exit`は`a0`の終了codeで終わり、戻らない。

## 同梱ゲスト例のlinkとbuild

linker scriptはimageの先頭を`0x0010_0000`に置き、`_start`を先頭sectionへ固定してentry addressを一致させる。
権限の違うsegmentがpageを共有するとloaderに拒否されるため、`.text`、`.rodata`、`.data`の間にpage境界を置く。
user stack (`0x3fff_0000`から`0x4000_0000`) とguard page (`0x3ffe_f000`) にはsegmentを置かない。

buildはリポジトリ直下から次のcommandで行う。

```sh
cargo build --manifest-path examples/guest-hello/Cargo.toml --target riscv64gc-unknown-none-elf --release --locked --config 'target.riscv64gc-unknown-none-elf.rustflags=["-C", "link-arg=-Tlinker.ld"]' --config 'build.target-dir="target/guest-hello"'
```

成果物は`target/guest-hello/riscv64gc-unknown-none-elf/release/guest-hello`である。
exampleはworkspaceから独立したpackageであり、独自の`Cargo.lock`を追跡してGit dependencyを再現可能にする。
`cargo xtask check`はworkspace buildの前に同じcommandでexampleをbuildするため、省略できない。
