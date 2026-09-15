# セキュリティテスト

MiniBundle parserとUART control-frame decoderは未信頼byte列を扱うため、
境界テストと手書きcaseに加えてfuzz harnessで検証する。
harnessは標準libraryのみで動き、seedとcorpusが同じなら同じ入力列を生成する。

## 対象と検出器

対象は次の二つのparserである。

- `bundle`: `minicontainer_bundle::parse`。
- `uart`: `minicontainer_protocol::Decoder`。入力から決定性に分割点を導き、
  複数chunkに分けて投入する経路も含む。

検出するのはpanic、hang、過大allocationの三つである。
parserが返す通常のerrorはfindingにしない。

## 使い方

```sh
cargo xtask fuzz --target bundle
cargo xtask fuzz --target uart --seed 7 --iters 50000 --time-limit 300
```

findingがあると入力を`target/fuzz/`へ保存し、終了code 1で再現commandを表示する。

```sh
cargo xtask fuzz --target uart --input 'target/fuzz/uart-000000000000001b-1234.bin'
```

`--input`は変異なしで一つのfileをそのまま再生する。
CIではchecked-in corpusの決定性replayをxtask単体テスト段階で実行する。

## 制限

| 項目 | 既定値 | 指定方法 |
| --- | --- | --- |
| 乱数seed | 27 | `--seed` |
| 入力数 | 10000 | `--iters` |
| 入力上限 | 128 KiB | `--max-bytes` |
| 入力あたりtimeout | 5秒 | `--input-timeout` |
| 全体の制限時間 | なし | `--time-limit` |
| 入力あたりallocation上限 | 16 MiB | 固定 |

入力上限は生成入力とcorpus seedの切り詰めに使う。
replayはfileを切り詰めず、そのまま実行する。

## findingを回帰テストへ固定する

1. 表示された再現commandでfindingを再確認する。
2. 入力を可能な範囲で縮小し、`xtask/corpus/<target>/`へ内容由来の名前で追加する。
3. `cargo test -p xtask --locked --test fuzz_corpus`でreplayを確認する。
4. 修正後はfocused testと`cargo xtask check-host`を実行する。

## corpus保存方針

- seedはKB単位の小さなfileに保つ。正常系の最小形と、既知の異常形
  (切詰め、不正magic、不正length、空入力)を両方置く。
- 追加前に内容を確認してから`git add`する。
  公開条件の検査は非UTF-8のfileを読み飛ばすが、目視確認は省略しない。
- 互換対象のcorpusは、互換方針の確定後に追加する。

## 対象外

- QEMU全体のfuzz。
- 無期限CI job。
- coverage値の保証。
