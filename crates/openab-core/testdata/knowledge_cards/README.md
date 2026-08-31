# Knowledge card regression fixtures

These fixtures preserve minimal, sanitized agent outputs that previously broke
the Discord knowledge-card boundary.

- Put payloads that must render as cards in `valid/`.
- Put payloads that must fail closed in `invalid/`.
- Use `.txt` files and keep each fixture to the smallest reproducing payload.
- Replace titles, authors, user names, Discord IDs, Notion IDs, UUIDs, hashes,
  and URLs with structurally valid example values.
- Never store full transcripts, tool output, OAuth callbacks, tokens, cookies,
  authorization headers, or other production credentials.

When an incident occurs:

1. Copy only the final agent output that reaches the card parser.
2. Remove unrelated prose and minimize it without removing the failure.
3. Replace every user or production identifier with example data.
4. Add the fixture to `valid/` or `invalid/` according to the intended result.
5. Run `make openab-test-fast ARGS="--lib knowledge_card_fixtures"` from the
   outer `remote-with-openab` repository, followed by `make openab-test`.

Valid fixtures may use `EXPECT_RENDER` for content that must survive rendering
and `DO_NOT_RENDER` for content that must not appear in the rendered card.
Invalid fixtures should use `DO_NOT_RENDER` for untrusted content that the
fallback response must suppress.
