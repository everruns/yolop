# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in Yolop, please report it responsibly.

**Do not open a public GitHub issue for security vulnerabilities.**

Instead, please email security issues to: **security@everruns.com**

Include:

- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Any suggested fixes (optional)

## Response Timeline

- **Acknowledgment**: Within 48 hours
- **Initial assessment**: Within 7 days
- **Resolution target**: Within 30 days for critical issues

## Scope

This security policy applies to:

- The `yolop` crate and binary
- Official documentation and examples in this repository

## Security Model

Yolop is a terminal coding agent that can read and write files, run shell
commands, and call configured model providers. Key security boundaries:

| Boundary | Protection |
| --- | --- |
| Filesystem | Workspace-rooted file access with a write blocklist for generated, dependency, build, environment, and VCS directories |
| Shell | Workspace-rooted `bash -lc` execution with a 120 s wall-clock timeout and 1 MiB per-stream output cap |
| Approvals | Optional `--ask` mode prompts before writes, edits, deletes, and shell commands |
| Sessions | Per-session logs are stored under the platform-native user data directory with owner-only permissions on Unix |
| Secrets | Provider credentials are read from process environment variables and should be supplied through a secret manager such as Doppler |

## Supported Versions

| Version | Supported |
| --- | --- |
| 0.1.x | Yes |

## Acknowledgments

We appreciate responsible disclosure and will acknowledge security researchers
who report valid vulnerabilities with permission.
