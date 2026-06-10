---
title: neon CLI Reference
description: Command reference for the neon command-line interface.
---

The `neon` CLI is the primary interface for interacting with NeonOS from your terminal.

:::note
The CLI is under active development. Commands documented here reflect the planned Phase 1 interface.
:::

## Global Flags

| Flag | Short | Description |
|------|-------|-------------|
| `--help` | `-h` | Print help for any command |
| `--version` | `-V` | Print the current `neon` version |
| `--verbose` | `-v` | Enable verbose/debug output |

---

## `neon`

Prints the top-level help and lists available subcommands.

```sh
neon
```

**Output example:**

```
neon — AI-native developer environment

Usage: neon <COMMAND>

Commands:
  doctor    Check your environment for required tools and configuration
  help      Print help

Options:
  -h, --help     Print help
  -V, --version  Print version
```

---

## `neon doctor`

Inspects your local environment and reports the status of required tools, runtime versions, and configuration.

```sh
neon doctor
```

**What it checks:**

- Node.js version (requires 22.12.0+)
- pnpm version
- Rust toolchain (reads `rust-toolchain.toml`)
- Cargo availability
- Git configuration
- Required environment variables

**Output example:**

```
neon doctor

  Node.js   22.14.0   OK
  pnpm      9.15.0    OK
  Rust      1.85.0    OK
  cargo     1.85.0    OK
  git        2.47.0   OK

All checks passed.
```

If any check fails, `neon doctor` exits with a non-zero status code and prints a remediation hint.
