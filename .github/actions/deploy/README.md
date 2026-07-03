# EdgeZero deploy action

Pre-release composite action for deploying a checked-out EdgeZero application to Fastly Compute.

```yaml
- uses: stackpop/edgezero/.github/actions/deploy@<full-commit-sha>
  with:
    adapter: fastly
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

See `docs/guide/deploy-github-actions.md` for the full contract, examples, and security guidance.
