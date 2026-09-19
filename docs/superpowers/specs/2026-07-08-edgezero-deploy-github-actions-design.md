# EdgeZero Deploy GitHub Actions — Superseded Design

**Status:** Superseded on 2026-09-17

**Date:** 2026-07-08

This design described the former standalone CLI artifact and deploy-time build
workflow. The current lifecycle consumes one immutable application release
containing the application CLI, package, and manifests. The deployment target
selects runtime resources but cannot select or rebuild application bytes.

Use these current documents:

- [GitHub Actions deployment guide](../../guide/deploy-github-actions.md)
- [Deployment action adoption](../../guide/deploy-action-adoption.md)
- [Fastly logical resource links design](./2026-09-17-fastly-logical-resource-links-design.md)

Git history retains the superseded design for historical review.
