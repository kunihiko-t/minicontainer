# OCI interop fixtures

Offline layouts produced by external tools from MiniContainer's golden
export. CI imports these layouts to prove that third-party interpretation
differences are detected. No network access is needed to use them.

## Contents

- `oras-copy/`: ORAS 1.3.4 `cp` output (tag `hello`). Drops the index
  `platform` hint and adds a `ref.name` annotation.
- `skopeo-copy/`: Skopeo 1.24.0 `copy` output. Drops the index `platform`
  hint and all annotations.
- `SHA256SUMS`: checksums of every fixture file. CI verifies them with
  `sha256sum -c` before use.

Both layouts carry the same blobs as the golden export: config
`3f19ea53…ac98e` (197 bytes), manifest `f095c935…0995b` (411 bytes), and
the MiniBundle layer `6c710671…96136` (136 bytes). Importing either
layout must restore bundle digest
`f8200f06e25dcb0f36a744b780c0fa1b40030c9a6bb9786b122cad44cad221ff`.

## Provenance

Generated from a debug `minictr` build of this repository:

```sh
python3 -c "open('/tmp/elf.bin','wb').write(bytes(range(16)))"
minictr image build --store /tmp/store hello /tmp/elf.bin
minictr image export-oci --store /tmp/store hello --output /tmp/layout
oras cp --from-oci-layout /tmp/layout@sha256:f095c9356a85036bc3e4a08e562e6826321aedd3569aaa15e076a9639170995b --to-oci-layout oras-copy:hello
skopeo copy oci:/tmp/layout oci:skopeo-copy
```

Tool versions: `oras version` 1.3.4, `skopeo --version` 1.24.0.

## Update procedure

Regenerate only when the v1 mapping changes (new media types or new
mandatory fields). Keep the fixture input identical (name `hello`, no
arguments, ELF bytes `0x00` through `0x0f`):

1. Build `minictr` and reproduce the provenance commands above.
2. Confirm both layouts import to the same bundle digest.
3. Refresh checksums: `(cd tests/fixtures/oci-interop && find oras-copy skopeo-copy -type f | sort | xargs sha256sum > SHA256SUMS)`.
4. Record the new tool versions in this README and in the
   [compatibility table](../../../docs/guide/12-oci-image-spec.md).

## Non-goals

The fixtures assert byte-level blob agreement and successful import.
They do not assert that annotations, tags, or unknown fields survive:
import drops them by design (see the mapping reference).
