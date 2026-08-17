# Sending Images Back to Discord

OpenAB relays PNG files that already exist in the current session workspace.
The agent does **not** call the Discord API and does **not** need a bot token.

Begin the final response with one directive per image:

```text
[[attach:artifacts/preview.png]]
Here is the generated preview.
```

Paths must be workspace-relative. The Discord adapter validates the canonical
workspace boundary, file size, PNG signature, and dimensions, then uploads with
OpenAB's own bot connection.

The canonical form above is required. OpenAB also accepts a leading `:` / `：`
or a short image/attachment label on the first directive, such as
`圖片：[[attach:artifacts/preview.png]]`. Other prose prefixes are left
untouched so documentation examples are not uploaded by accident.

Do not put `[[attach:...]]` at the end of a sentence. A single embedded
directive after explanation text is ignored.

Limits: PNG only, at most 4 images, 10 MiB each, 20 MiB total.

> For non-PNG files (PDF, CSV, logs), see [sendfiles.md](sendfiles.md). Native
> workspace relay currently covers PNG only.
