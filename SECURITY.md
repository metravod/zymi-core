# Security Policy

## Reporting a vulnerability

zymi-core is an agent engine whose whole premise is an auditable, event-sourced
execution trail — so security reports, especially ones touching the audit trail,
approvals, or the policy/contract boundaries, are genuinely welcome.

**Please report privately, not in a public issue or discussion:**

- Email: **arttrek42@gmail.com** — subject line starting with `[security]`.

Include enough to reproduce: the affected file/line if you have it, the version
or commit, and the impact as you see it. If you have a suggested fix or a
severity read, even better.

### What to expect

- An acknowledgement within a few days (this is a solo-maintained project, so
  not instant, but you will hear back).
- An honest reply: whether the finding is confirmed, and if we disagree, why.
- Fixes shipped in a normal release, with the specific commits linked in the
  release notes and credit to you (see below).

### Disclosure

Please keep specifics out of public channels until a fix is released. Once it
is out, full public detail — commits, root cause, credit — is the goal, not the
exception. If a finding is not security-sensitive, a normal GitHub issue is
perfectly fine.

## Acknowledgements

Researchers who have responsibly reported issues that improved zymi-core:

- **arikusi** (via Hacker News) — an in-depth read of the event store, approval
  flow, policy engine, and LLM clients that drove the 0.7.x / 0.8.0 audit-trail
  and contract-boundary hardening.
