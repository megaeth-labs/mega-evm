# Security Policy

## Scope

The following are considered security-relevant vulnerabilities:

- Consensus bugs or state transition correctness issues
- Gas model errors (compute gas, storage gas, resource limit miscalculations)
- System contract vulnerabilities
- Opcode behavior deviations from spec
- State corruption or non-determinism
- Exploitable resource limit bypasses

Out of scope: CLI tool usability, documentation errors, test-only code, and non-consensus tooling.

## Supported Versions

| Version | Supported |
| --- | --- |
| `main` branch | Yes |
| Latest stable spec (`REX4`) | Yes |
| Older specs | No |

## Reporting a Vulnerability

**Do not open a public GitHub issue for security vulnerabilities.**

Instead, report through one of the following private channels:

- **Email**: [security@megaeth.com](mailto:security@megaeth.com)
- **GitHub Security Advisories**: [Report a vulnerability](https://github.com/megaeth-labs/mega-evm/security/advisories/new)

Please include:

- A description of the vulnerability and its potential impact
- Steps to reproduce or a proof of concept
- The affected spec(s) and code paths, if known

## Response Timeline

| Stage | Timeframe |
| --- | --- |
| Acknowledgement | Within 48 hours |
| Initial triage and severity assessment | Within 1 week |
| Fix development and internal review | Within 30 days (critical issues prioritized) |
| Coordinated public disclosure | Within 90 days of report, or upon fix release |

We may request an extension of the disclosure deadline for complex issues.
Reporters will be kept informed throughout the process.

## Disclosure Policy

We follow coordinated disclosure.
Please do not publicly disclose the vulnerability until we have released a fix or the 90-day disclosure window has elapsed, whichever comes first.

We credit reporters in the advisory unless they prefer to remain anonymous.
