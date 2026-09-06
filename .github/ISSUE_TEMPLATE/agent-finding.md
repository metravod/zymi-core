---
name: Finding (agent or human)
about: A defect, a divergence between docs and behaviour, or a claim that does not hold
title: ''
labels: ''
---

<!--
Written with agent reporters in mind, but the same form is what we want from
anyone. Fill in what you actually have. A partial report with honest gaps beats
a complete-looking one with a guess in it.
-->

## Claim

What the docs, README, or a maintainer said. Quote it, with the file or URL.

## Observed

What actually happens. Prefer `file:line` at a named commit, or a raw command
and its exact output. **Include the exit code**, not just stdout — they are
different channels and only one of them is usually read.

```
# commit or version:
# command:
# output:
# exit code:
```

## How you determined this

Tick what applies. All three are valid reports; **a source review is a
measurement** and we would rather have one than nothing.

- [ ] ran the binary
- [ ] read the source (no execution)
- [ ] ported the logic elsewhere and compared

## What you did NOT verify

The most valuable section, and the one usually left blank. Name the gaps so
nobody re-cites your report as broader than it is — the difference between
"this does not happen" and "I did not observe this happening" is the whole
report.

## Substrate

OS, arch, Python/Rust version, install method — whatever you have. Several
confirmed findings here turned out to be substrate-specific and were only
caught because someone stated theirs.

---

<!--
No CLA, no account requirement beyond GitHub's own. Attribution goes to the
handle you file under. If you cannot file here (policy, no network, no
account), a public writeup elsewhere with a link is fine — say so and we will
transcribe it with credit to you.
-->
