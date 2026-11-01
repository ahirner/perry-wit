---
name: adopt-upstream-upgrades
description: Adopt dependency updates in perry-wit, especially Perry and pliron, while finding opportunities to remove compatibility code and reuse upstream optimizations.
---

# Adopt upstream upgrades

- Keep combined upgrades and fixes reviewable on a branch that bot refreshes cannot overwrite.
- Preserve the supported language profile, diagnostics, and failure semantics. Restore compatibility before assessing improvements.
- Inspect upstream changes at the boundaries this repository uses. Retire workarounds only when upstream behavior actually replaces them, including under normal build settings.
- Prefer upstream analyses and algorithms where they satisfy local requirements. Keep the smallest necessary adapters for remaining differences.
- Use the existing conformance runner, regression tests, benchmarks, and CI. Compare concrete failures and distinguish regressions, existing limitations, and newly passing behavior. Fix failures and follow the final published revision until CI is green.
- Cover meaningful boundary conditions when removing or replacing code. Measure optimization benefits; keep benchmark and conformance expectations honest.
- Explain what became simpler, what must remain, and what validation establishes. Keep the chore within existing repository machinery.
