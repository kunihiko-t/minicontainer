# guest-echo

MiniContainerで動くstdin echoのゲスト例である。
Guest ABIの`read`でstdin staging (最大4 KiB) から読み、byte列をそのまま標準出力へ書き戻す。hostからのEOF frameで`read`が0を返すと終了コード42で終わる。

## build

リポジトリ直下から実行する。

```sh
cargo build --manifest-path examples/guest-echo/Cargo.toml --target riscv64gc-unknown-none-elf --release --locked --config 'target.riscv64gc-unknown-none-elf.rustflags=["-C", "link-arg=-Tlinker.ld"]' --config 'build.target-dir="target/guest-echo"'
```

成果物は`target/guest-echo/riscv64gc-unknown-none-elf/release/guest-echo`である。
このdirectory内で実行する場合は、`cargo build --release --locked`の短縮形も使える。

## ABIの読み方

system call番号とfile descriptorは[source](src/main.rs)内の`minios_abi::syscall`参照だけが定義元である。
register配置とlink addressの説明は[第3章](../../docs/guide/03-static-elf-abi.md)を参照する。
EOFと`read`の振る舞いは[第7章](../../docs/guide/07-uart-runtime.md)を参照する。
