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
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cd webui && npx tsc --noEmit && npm run build
docker build -t zed-proxy:local .
```

## Checklist

- [ ] `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` pass, and new behaviour has a test.
- [ ] `cd webui && npx tsc --noEmit && npm run build` passes when the Web UI changed.
- [ ] `docker build -t zed-proxy:local .` succeeds, or the change cannot affect the image.
- [ ] No keys, tokens, account credentials or `accounts.json` content are included in the diff, logs or fixtures.
- [ ] Documentation is updated in the same change, in both languages (`README.md`, `README.zh-CN.md`, `docs/*.md` and `docs/*.zh-CN.md`, `CHANGELOG.md` as applicable).
- [ ] Files use LF line endings (`git ls-files --eol` shows `i/lf`).
