# Changelog

All notable changes to `nanocodex-oai-api` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Tower-native OpenAI Responses transports, typed protocol values, retry and
  reconnect policy, and full-fidelity tracing.
- Instruction-bound managed sessions with authoritative typed history,
  incremental continuation, atomic compaction, and completed-only commits.
- Responses that are both typed streams and awaitable completed aggregates.
