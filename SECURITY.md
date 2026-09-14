# Security Policy

## Supported versions

Security fixes are applied to the latest release and the `main` branch.

## Reporting a vulnerability

Do not open a public issue for an undisclosed vulnerability.
Use GitHub's private vulnerability reporting for this repository.

## Security boundary

MiniContainer is an educational runtime and is not a production security boundary for untrusted workloads.

## Release artifacts

Distribution archives are attached to the tag's GitHub Release with signed
build provenance. Before installing, verify the checksum and then confirm the
artifact was built by this repository's release workflow:

```sh
sha256sum -c 'minicontainer-<version>-x86_64-unknown-linux-gnu.tar.gz.sha256'
gh attestation verify 'minicontainer-<version>-x86_64-unknown-linux-gnu.tar.gz' --owner kunihiko-t
```

A checksum alone proves file integrity, not origin. The attestation binds the
artifact digest to the source commit and the builder workflow.
