## Summary

<!-- What does this change and why? One or two paragraphs. -->

## Related issue

<!-- Closes #123, or "none". -->

## Type of change

- [ ] Bug fix (`fix:`)
- [ ] New feature (`feat:`)
- [ ] Refactor (`refactor:`)
- [ ] Build or CI (`build:`)
- [ ] Documentation (`docs:`)
- [ ] Chore (`chore:`)

## Testing

<!-- What did you run, and what was the result? -->

```
zig build test
docker build -t zed-proxy:local .
```

## Checklist

- [ ] `zig build test` passes, and new behaviour has a test.
- [ ] `docker build -t zed-proxy:local .` succeeds, or the change cannot affect the image.
- [ ] No keys, tokens, account credentials or `accounts.json` content are included in the diff, logs or fixtures.
- [ ] Documentation is updated in the same change (`README.md`, `README.zh-CN.md`, `docs/architecture.md`, `CHANGELOG.md` as applicable).
- [ ] Files use LF line endings (`git ls-files --eol` shows `i/lf`).
