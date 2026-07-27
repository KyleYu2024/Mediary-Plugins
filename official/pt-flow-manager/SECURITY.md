# Security Policy

## Supported versions

Only the latest released version is supported with security fixes.

## Reporting a vulnerability

Please use GitHub private vulnerability reporting for this repository. Do not
include Mediary tokens, tracker cookies, downloader credentials, torrent
passkeys, or other secrets in a public issue.

The plugin does not read site cookies or downloader credentials directly. It
uses the scoped Mediary plugin API and stores only task configuration, managed
torrent metadata, and run diagnostics under its own plugin data directory.
