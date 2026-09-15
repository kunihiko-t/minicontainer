# Compatibility fixtures

Golden artifacts pinned to older contract versions. The compatibility
policy in `docs/reference/compatibility.md` defines what each fixture
proves; CI imports them to catch accidental breaking changes.

## Contents

- `minibundle-abi-v1.0.mcb`: the canonical MiniBundle built under Guest
  ABI v1.0 (`BOOT_ABI_MINOR=0`): name `hello`, no arguments, ELF bytes
  `0x00..0x0f`, 136 bytes total. Its bundle digest is
  `f8200f06e25dcb0f36a744b780c0fa1b40030c9a6bb9786b122cad44cad221ff` and
  its whole-file SHA-256 is
  `6c710671215c4929fa600956d236ae82df1f3a9f05f0b125c3fb0feed9096136`.
- `SHA256SUMS`: checksums of every fixture file. CI verifies them with
  `sha256sum -c` before use.

## Provenance

The bundle bytes are the layer blob from `../oci-interop/oras-copy`,
produced by a `minictr image build` of the golden input on Guest ABI
v1.0 and copied through ORAS. Importing this file must succeed on any
host that claims v1.x compatibility, and the store must report the
bundle digest `f8200f06…` above.
