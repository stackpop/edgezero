# Build Cache Consumer and Adoption Implementation Plan (plan 5 of 5)

> **Execution:** Start after plan 4 is merged and its gate revision is active. Select candidate exact
> version `C` before opening the final executable candidate. Select and publish stable `V` only after
> that merged commit passes detached local and immutable-candidate hosted verification; release the
> runnable documentation afterward as protected revision `R`.

**Goal:** Ship the two-job producer/consumer workflow, prove real app repositories can adopt it with
full-SHA app identity and explicit inputs, publish exact stable action version `V`, then activate
synchronized runnable documentation at revision `R` without ever merging an unpublished version ref.

**Spec:** `docs/superpowers/specs/2026-08-20-edgezero-deploy-build-caching-design.md` v6.29 Sections
3.3, 5.1, 5.2, 5.4, 7, 9, and 10, plus the parent deploy lifecycle contract.

## 1. Release structure

- `C` is an unused canonical exact patch version with no prerelease suffix. After the final candidate
  reaches protected main as commit `H`, `C` is published as an immutable GitHub Release at `H` with
  `prerelease:true` so hosted cross-repository workflows can test the exact merged bytes while every
  `uses:` ref remains a full version.
- `H` becomes final action revision `P` only after the complete clean-detached local suite at `H` and
  complete hosted suite through literal `C` pass. Only then is distinct unused canonical stable
  version `V` selected and published as an immutable release at exact `P`.
- `R` is a later documentation-only protected-main commit. After the literal-`V` hosted smoke passes,
  it adds canonical `docs/.edgezero-action-release.json` bound to `{V,P}` and replaces the bootstrap
  placeholder in tracked Markdown with literal `V`. It changes no action/workflow implementation or
  gate-owned path, and it is not a new action revision.
- Repository immutable releases must report enabled, and exact no-bypass `refs/tags/v*` rules prevent
  action-version tag update/deletion. Failed candidate versions remain immutable; a correction uses a
  new commit and newly selected unused `C`. A failed published stable version is never
  retargeted; it is superseded by a new patch. Deletion of a GitHub Release object or mutation of its
  title, notes, prerelease, or latest metadata by a sufficiently privileged actor remains an accepted
  availability/discovery risk, so final evidence preserves release attestations and audits release
  existence/state; tag deletion and retargeting remain blocked by the no-bypass ruleset.
- The gate built in plan 1 owns the entire bootstrap-to-released documentation transition. Before
  `R`, only the four named prepublication documents may use the exact placeholder and are not claimed
  runnable. At and after `R`, all tracked Markdown is placeholder-free and every EdgeZero `uses:` ref
  names literal stable `V`.

## 2. Gate update and contract fixtures

- [ ] Add parsed-workflow tests for exact two-job separation, canonical exact patch-version
      reusable-workflow/action refs, unique artifact names, caller and action identity forwarding,
      public identity-action use, action-local source materialization with no authority-path handoff,
      no aggregate matrix identity, minimal permissions, secret confinement, and absence of
      direct-composite producer use. Classify jobs by parsed AST shape. Require literal
      `runs-on: ubuntu-24.04` in the reusable producer and every step-based consumer job containing a
      `steps[*].uses` public EdgeZero action reference. Require a job-level EdgeZero reusable-workflow
      caller with `jobs.<id>.uses` to have neither `steps` nor `runs-on`. Reject mixed shapes, absent or
      dynamic step-based labels, `ubuntu-latest`, other standard, larger, custom-image, and self-hosted
      labels. Require the producer bootstrap, inline checkout verification, and shared runner helper
      in exact order without conditional, continuation, failure masking, pre-verification local
      execution, or non-cleanup always-run paths. Require the legacy path at candidate `H` to be only
      the exact non-producing retirement stub, and reject any workflow or documentation that invokes
      it as a producer.
- [ ] Add public/private app repository fixtures, root/nested workspace fixtures, public Git and
      sibling path dependencies, submodules, no-filter and pinned-LFS cases, app-env migration,
      generated outputs, production/staging provider paths, and cache enabled/disabled cases.
- [ ] Add negative fixtures for major/minor/prerelease/SHA/branch refs in published workflows, mixed
      EdgeZero versions, producer provider inputs, missing authority/Copy B identity check, authority
      path/descriptor/handle passed between actions, checkout token passed to a source-free action,
      artifact-name reuse, caller-supplied platform/expected identity, ambient env reliance, custom
      Git filters, and legacy `--stage`.
- [ ] Land and activate this gate update before changing reusable workflow/action interfaces. The gate
      validates version grammar and equality, not a not-yet-created value for `V`.

## 3. Final reusable producer workflow

- [ ] Freeze `.github/workflows/build-app-cli.yml` as a build-only `workflow_call` interface with the
      exact design inputs, required `rust-toolchain`, `app-checkout-token` secret, hosted-only workflow
      version/resolved-SHA identity checks, literal `runs-on: ubuntu-24.04`, the first-step inline
      producer bootstrap, bounded timeout, cache default `false`, and no provider credential or
      mutation surface.
- [ ] Materialize EdgeZero source separately at exact `job.workflow_sha`. Checkout the app into a
      recursive non-sparse authority root at full lowercase `app-ref`, verify authenticated repository
      id and CallerExpectedIdentity, remove credentials, enforce the no-filter-or-pinned-LFS contract,
      and invoke the shared exporter to create non-hardlinked `.git`-free Copy A. Prove no local action
      or helper resolves from app data.
- [ ] Compile/package through the digest-pinned image and upload one deterministic named artifact.
      Reverify the authority after use and remove Copy A plus every operation root on all exits.
- [ ] Emit only `artifact-name`, trusted `action-version`, resolved `action-revision`, and the five
      CallerExpectedIdentity fields. Do not expose host paths, image ref/digest, protocol, cache path,
      token, provider state, or an aggregate matrix value.
- [ ] Preserve separate cache lookup and save decisions: cross-repository cache use without disclosure
      acknowledgement fails before restore; save additionally requires the protected-event predicate.
      `cache:false` uses the no-sccache `uncached-compile` profile and Cargo has exactly one compile/
      build invocation after metadata preflight.

## 4. Consumer job topology

- [ ] Provide tested workflows in which job 1 calls the reusable producer and job 2 invokes public
      `compute-app-cli-identity` at the same action version with the exact
      repository/ref/id/workspace/cwd/package/bin/toolchain inputs and `app-checkout-token`. Compare
      all five typed outputs with the producer and only then invoke provider actions with the named
      artifact. Every step-based consumer job containing a public EdgeZero action reference declares
      literal `runs-on: ubuntu-24.04`; the caller job with `jobs.<id>.uses` that directly invokes the
      reusable workflow has neither `steps` nor `runs-on`, and the called workflow owns its literal
      label. The identity action destroys its action-private authority before returning and exposes no
      host path or handle.
- [ ] Pin every EdgeZero reusable workflow and action within a published consumer workflow to one
      identical literal stable `V`. Generated candidate-release tests substitute one identical literal
      `C`. Reject mixed versions, major/minor tags, prereleases in published examples, SHAs, and
      branches even when each ref resolves.
- [ ] Pin every third-party action to a separately reviewed canonical stable patch version. Record its
      release URL and resolved commit in release evidence and prove no same-named branch exists at
      review time. Later version-tag movement/deletion or same-name branch ambiguity introduced by a
      trusted third-party publisher is accepted risk; branches, major/minor tags, prereleases, commit
      SHAs, and mutable Docker tags remain prohibited in repository text.
- [ ] Pass producer `action-version` and `action-revision` to the comparison step. Every EdgeZero
      composite requires its runner-provided action repository/ref to equal
      `stackpop/edgezero@<action-version>` before downloading anything. EdgeZero immutable release and
      tag protections bind `V` or `C` to the producer revision.
- [ ] Give each matrix leg a deterministic unique artifact name and keep its identity comparison in
      that leg. Reject aggregation, `merge-multiple`, wildcard downloads, and outputs inferred from a
      matrix-wide reusable-workflow call.
- [ ] Keep checkout tokens host-side and provider tokens only in consumer steps that require them.
      Pass `app-checkout-token` only to the identity action and the exact source-bearing actions
      `deploy-fastly` and `config-push-fastly`; each independently materializes its authority and
      removes its checkout credential channel before app code or provider-token creation/injection.
      Source-free actions receive neither source-materialization inputs nor the token. Define explicit
      job permissions and prove artifacts, caches, summaries, logs, and outputs contain neither token.
- [ ] Exercise `validate-app-cli-provenance`, `active-version-fastly`, `deploy-fastly`,
      `healthcheck-fastly`, `rollback-fastly`, and `config-push-fastly` as independent consumers. Each
      action downloads its own artifact, derives PlatformIdentity locally, writes a fresh expected file
      from verified caller plus local platform fields, validates/smokes, and cleans its private state.
      `deploy-fastly` and `config-push-fastly` additionally rematerialize and verify independent
      action-local authorities; they never reuse the identity action's destroyed authority.

## 5. App-repository migration behavior

- [ ] Replace ambient workflow `env` examples with the explicit duplicate-safe `app-env` JSON object.
      Document the exact deny rules and state that otherwise allowed values are caller-classified as
      non-secret and may affect cross-repository compilation cache contents.
- [ ] Add `generated-output-paths` only for absent repository-relative roots genuinely written by the
      credential-free app build. Explain that selected Fastly project `bin` and `pkg` roots are
      implicit, callers must not list them, and preexisting/overlapping/tracked-containing roots fail.
- [ ] Require full app commit SHA, canonical repository id, explicit workspace root/package/bin and
      `rust-toolchain`, and a tracked lockfile. Cover nested workspaces and private authority checkouts
      without suggesting app branch or tag refs.
- [ ] Document the exact consumer authority interface: `compute-app-cli-identity` returns only five
      typed identity fields; source-bearing actions receive the same explicit source inputs plus the
      checkout token and rematerialize independently; source-free actions receive neither. Do not
      document or expose an authority path, checkout step output, or reusable handle.
- [ ] State that the caller repository's effective Actions policy must permit version-tag action and
      reusable-workflow refs; an organization/enterprise full-SHA mandate is incompatible with this
      release policy and must fail adoption preflight rather than trigger an undocumented SHA fallback.
- [ ] State that v1 supports only step-based consumer jobs containing public EdgeZero action
      references with literal `runs-on: ubuntu-24.04`, plus job-level reusable-workflow callers that
      omit `steps` and `runs-on` and delegate to the producer's identical internal label. Other
      standard Ubuntu labels, `ubuntu-latest`, larger runners, custom GitHub-hosted images, and
      self-hosted runners are outside the compatibility contract; the action's runner-context
      predicate is still required because the label is not security evidence and is not observable
      from a composite action.
- [ ] Explain that protocol 1 rejects custom Git filters, supports only no filter or the action's
      pinned Git LFS materialization path, rejects repository/enclosing Cargo config and credentials,
      requires the image toolchain, permits only public dependency fetching, and preserves the
      accepted undeclared proc-macro/build-script cache risk.
- [ ] Keep caching opt-in and distinguish `build-app-cli.cache` from `deploy-fastly.cache`. Both require
      disclosure acknowledgement before cross-repository restore and use the same protected-event save
      predicate. The latter applies only to credential-free app-build, saves before token introduction,
      and does not prevent Fastly's token-bearing deploy compile.
- [ ] Preserve production/staging, first-deploy, healthcheck, rollback, cancellation, config-push, and
      mutation-attempt semantics from the parent guide. Every staged command uses `--staging`; no
      compatibility alias or legacy `--stage` instruction remains.

## 6. Candidate-independent integration preparation

- [ ] Create disposable public and private app repositories or equivalent GitHub-owned fixtures with
      immutable source SHAs. Validate their repository ids, authority/export state, required LFS cases,
      explicit app inputs, and provider test credentials before selecting `V`; do not replace hosted
      evidence with local `act` or Docker-only tests.
- [ ] Build a release harness that writes one exact EdgeZero version into every producer/provider ref,
      verifies all refs match, triggers literal `ubuntu-24.04` hosted linux/amd64 runs, polls exact
      run/job attempts, and records artifact/image/action/app identities without logging credentials
      or app-env values.
- [ ] Locally prove cold/warm/default-off cache behavior, source relocation, nested workspace identity,
      artifact transfer, identity-action/source-bearing authority separation, Copy A/Copy B
      independence, expected-file freshness, exact caller/platform/action validation, sorted
      placeholder-only `env -S` launch argv, and exact post-`env -i` target environments using
      generated fixtures. Every public action fixture also covers missing/malformed context bindings
      and self-hosted Linux/X64 rejection before artifact, source, Docker, or token work.
- [ ] Prepare production/staging provider fixtures for successful deploy, unhealthy rollback, first
      deploy, active-version, healthcheck, config push, and cancellation reconciliation. Assert no
      mutation on identity, source-freeze, output-root, loader, or token-order failure.

## 7. Build final candidate `H`

- [ ] Query current releases and remote refs, select unused canonical patch version `C`, and validate
      its grammar. Require no current Git ref or release with that `tag_name`; release display `name`
      is not identity and supplies no historical proof. Record the queries and results, and require
      later creation to succeed without tag reuse. Do not create the tag yet, and do not select `V`.
- [ ] Reconcile the parent spec, original implementation plan, adoption guide, and public guide
      against shipped metadata: two-job topology, authority materialization, action/caller identity,
      exact app inputs, `app-env`, generated outputs, cache defaults, provider lifecycle, and literal
      `runs-on: ubuntu-24.04` on every step-based consumer job containing a public EdgeZero action
      reference, with no `runs-on` or `steps` on a job-level reusable-workflow caller. Keep the exact
      `<EDGEZERO_ACTION_VERSION>` bootstrap placeholder in these four prepublication documents; do
      not introduce a guessed or unpublished stable version.
- [ ] Run plan 1's permanent documentation scanner in bootstrap mode over every tracked Markdown
      file. Prove `docs/.edgezero-action-release.json` is absent, the placeholder appears only in the
      four named surfaces, no other action-ref placeholder exists, and every third-party ref is an
      exact stable patch version. Require the exact step-based/reusable-caller AST and runner-label
      rules for every fenced consumer workflow. Do not modify the gate-owned scanner in this plan.
- [ ] Run every protocol, cache, image, launcher, source-freeze, provider, workflow, fixture, docs/pin,
      actionlint, zizmor, shellcheck, Rust, and local integration suite at one clean candidate descended
      from `B`. Confirm `image.json` remains reviewed `{D,S,protocol}`.
- [ ] Run independent contract and release-adversary reviews against design v6.29, including exact-tag
      policy, third-party tag movement risk, EdgeZero immutable releases, action-version mixing,
      substitution, identity replay, malformed artifacts, host/container races, source mutation,
      generated-output escape, credential flow, cache disclosure, rollback, and cancellation.
- [ ] Merge through the one-entry queue and require exact protected-main push checks. Record the
      resulting lowercase full commit SHA as `H`; do not designate it `P` or publish stable `V` yet.

## 8. Qualify `H` and publish `V`

- [ ] From a clean detached checkout at exact `H`, rerun the complete local suite from Section 7,
      including the bootstrap-state docs/pin scanner, exact staged container build, Docker-backed
      provider tests, and repository CI commands. Preserve logs and digests bound to `H`.
- [ ] Select the release operator before `C` exists. Use a short-lived fine-grained PAT selected only
      for `stackpop/edgezero`, expiring within 24 hours, with exactly repository `Contents:write` and
      `Workflows:write`, implicit metadata read, organization `Members:read`, and no other grant. Keep
      it outside Actions/argv/environment, reject classic or installation tokens, and preserve its
      settings as review evidence. The local helper permits only no-redirect versioned `GET /user`,
      exact organization-id/team-id membership GET, release POST, release-id PATCH, and release-id GET
      routes from the design. Require active team membership and no unallowlisted method/path/query/
      body field; destroy the token after release work.
- [ ] For each action release, create a draft with exact `tag_name`, full target commit, no assets,
      and required `prerelease` boolean; publish only by a PATCH that changes `draft` to false. Record
      requests/responses, authenticated login, release `author.login`, team membership, release id/
      URL, remote peeled ref, exact state, and generated attestation. A different actor, broader token,
      or direct unrecorded tag creation fails release.
- [ ] Reconfirm the immutable-releases endpoint returns HTTP 200 with `enabled:true` and boolean
      `enforced_by_owner`, and both action-version tag rulesets are exact. Publish candidate `C` as an
      immutable release targeting exact `H` with API field `prerelease:true`; require its exact
      patch-version `tag_name`, `draft:false`, `immutable:true`, and the remote peeled tag to resolve
      to `H`, and record the generated release attestation. Never move or delete a failed candidate
      tag.
- [ ] Run the complete hosted cross-repository suite with every EdgeZero workflow/action ref equal to
      literal `C`: cold/warm/default-off caches; public/private and LFS source; two matrix identities;
      artifact/identity/expected handoff; all provider lifecycles; all negative pre-mutation cases; and
      cancellation reconciliation. Verify exact run attempts and `action-revision==H`.
- [ ] If either exact-`H` suite fails, fix on a new commit, choose a new unused `C`, and repeat Sections
      7-8; no stable `V` has been selected. After both pass, designate `H` as final action revision `P`.
- [ ] Query current releases and remote refs, then select a distinct unused canonical stable patch
      version `V`. Require no current ref or release with that `tag_name`. Publish `V` through the same
      exact fine-grained-PAT actor/draft/publish procedure targeting `P`; require API and anonymous
      peeled-ref resolution to equal `P`, `draft:false`, `prerelease:false`, and `immutable:true`.
      Preserve the attestation/operator evidence and verify no major/minor alias was created or moved.
- [ ] Run a final hosted producer/consumer identity smoke with every EdgeZero ref literal `V` and
      require `action-revision==P`. A post-publication failure does not permit changing `V`; publish a
      corrected new patch through the normal protected process. Do not create documentation revision
      `R` until this smoke passes.

## 9. Publish documentation revision `R`

- [ ] Create canonical `docs/.edgezero-action-release.json` with exact JCS bytes
      `{"action-revision":"<P>","action-version":"<V>","schema-version":1}` and no trailing newline.
      Replace every `<EDGEZERO_ACTION_VERSION>` ref in tracked Markdown with literal `V`. Change only
      tracked Markdown plus that record; do not touch action/workflow code, metadata, gate-owned paths,
      or implementation fixtures.
- [ ] Run the plan-1 dual-state scanner on the documentation PR and its final merge-group candidate
      using the closed event-to-range table. The selected base is the then-current protected-main
      commit, which may be newer than `P`; the synthetic candidate is not named `R`. Require the
      one-way bootstrap-to-released transition, exact record schema/JCS, no placeholder in any fenced
      YAML, one identical EdgeZero `V` per workflow, exact step-based/reusable-caller job shapes and
      runner labels, and exact third-party patch versions. The hosted transition verifier must prove
      public release `V` is `draft:false`, `prerelease:false`, `immutable:true` and its API target and
      anonymously peeled ref both equal record `P`.
- [ ] Parse every fenced YAML example, validate it against action/workflow metadata, and prove examples
      are runnable after only repository/application-value substitution. Run the complete docs build,
      pin scanner, actionlint, and required repository checks; merge through the one-entry queue and
      record the resulting protected-main commit as `R`. Require the protected-main push scanner to
      compare `event.before` with `event.after==R`, then rerun released-state checks. Do not move or
      recreate `V`.

## 10. Final review

- [ ] Compare every documented input, output, default, secret, permission, mount, environment,
      artifact, cache, source-freeze, provider, and failure behavior at documentation revision `R` to
      actual action metadata/tests at `P`.
- [ ] Search for forbidden external major/minor/prerelease/branch/SHA `uses:` refs, EdgeZero version
      mismatches, direct `build-app-cli` composite producer guidance, caller-provided platform or
      expected fields, unsupported runner labels, ambient app env, writable source mounts outside
      declared roots, and legacy `--stage`. Every hit must be a clearly marked rejected example or
      fail documentation release.
- [ ] Verify public anonymous image pull by digest and an end-to-end fresh app adoption from the
      published guide using literal `V`. Record image-release gate `G`, the ordered gate-rotation
      lineage and final active gate, `{S,D,B,P,C,V,R,protocol}`, action-version resolved commits,
      release attestations, hosted run ids/attempts, and documentation-check results. Do not collapse
      the post-`B` gate revisions into the image-release `G` label.

**Gate:** exact merged revision `P` passed the complete local and candidate-version hosted suites;
immutable stable release `V` resolves to `P`; protected documentation revision `R` activated the
one-way released scanner state and all concrete consumer examples use `V`; no consumer relies on a
floating ref, same-job producer shortcut, ambient application environment, or provider mutation
before independent validation.
