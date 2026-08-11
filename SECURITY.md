# Security policy

## Supported versions

Only the latest released minor version receives security fixes.

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability. Use GitHub's private
security advisory form for this repository instead. Include the affected
version, reproduction steps, impact, and any suggested mitigation.

Reporch will acknowledge a complete report as soon as practical, coordinate a
fix and disclosure date, and credit reporters who want attribution. Do not test
against accounts, projects, or infrastructure you do not own.

## Release trust

Official releases are produced only by the repository's protected GitHub
Actions workflow. npm packages use provenance, contain no install scripts, and
select an npm-delivered native binary for the current platform. Release assets
include SHA-256 checksums, an SPDX SBOM, and GitHub artifact attestations.
