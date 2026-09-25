# EdgeZero Deploy Actions — Superseded Build Caching Design

**Status:** Withdrawn on 2026-09-17

**Date:** 2026-08-17

This proposal cached builds inside a deployment workflow. The current lifecycle
does not build application code: a credential-free release producer creates one
immutable application release, and publisher workflows consume those exact
bytes.

Any future build-cache design belongs to that release producer and must remain
independent of a particular application or deployment repository.

Use these current documents:

- [GitHub Actions deployment guide](../../guide/deploy-github-actions.md)
- [Deployment action adoption](../../guide/deploy-action-adoption.md)
- [Fastly logical resource links design](./2026-09-17-fastly-logical-resource-links-design.md)

Git history retains the withdrawn proposal for historical review.
