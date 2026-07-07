# Runbay — Design (v0.1 draft)

## 1. Overview

Runbay is a local-first runtime daemon (`runbayd`) that spawns, supervises, and isolates coding-agent sessions on a developer workstation over a loopback HTTP + SSE interface.

## 2. Core Principles

- **Durable logical sessions, ephemeral OS processes.** Idle processes can be reaped while preserving runtime-native session continuity.
- **Honest capability tiers.** Adapters advertise what each runtime genuinely supports (`duplex`, `resumable`, or `one_shot`).
- **Single enforcement boundary.** Sandbox isolation, credential proxying, and audit telemetry live in one local daemon.
