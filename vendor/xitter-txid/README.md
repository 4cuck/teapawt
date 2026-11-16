# xitter-txid

> Patched copy of [`xitter-txid` 0.2.0](https://github.com/amaanq/xitter-txid).

The only change is in `src/transaction.rs::compute_animation_key`.

X's client reads the `(e[N], 16)` list in the `ondemand.s` chunk as
`[row, a, b, c]`: the first number names the key byte whose remainder picks the
animation row, and only the other three multiply into the animation time. 0.2.0
took the row from key byte zero and multiplied all four indices, so it built a
different animation key from the same page.

A wrong animation key does not fail locally — it produces a well-formed
transaction ID that X refuses. Every `POST` to `api.x.com/1.1/account/settings.json`
came back as a bodyless `404` with code `34`, which is what X answers a
transaction ID it will not accept. That is why the settings writes that turn off
"Display media that may contain sensitive content" and the two search-safety
toggles never landed, and why every imported cookie account kept hiding NSFW
posts.

Verified against a live session: with the fix the same account and the same
page answer `200` with `"display_sensitive_media":true` in the body; reverting
the change puts the `404` code `34` back.

Drop this copy once the crate is fixed upstream.