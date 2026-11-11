# Security policy

spared is an early preview (0.0.x). Only the latest release is supported.

## Reporting a vulnerability

Report it privately. Open the repository's Security tab and choose "Report a vulnerability". Do not open a public issue or pull request for a security problem.

Include the version (`spared --version`), what you ran, and what happened. A failing app or a command sequence that reproduces it is the most useful report.

## Scope

In scope:

- A guest app reading or writing outside its own data directory, or reaching a host function, file, network address or environment variable its manifest did not grant.
- An app exceeding its memory, CPU, storage or request limits in a way the host does not stop.
- Authentication or token handling in `spared serve` that lets a caller act without the token.
- Release artifacts that do not match their checksums or attestations.

Out of scope: denial of service by an operator who controls the host, problems that need the lab build (`--features lab`), and vulnerabilities in a dependency that spared does not reach (report those upstream).
