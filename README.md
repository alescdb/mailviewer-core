# mailviewer-core

Reading and sanitizing of `.eml` and Outlook `.msg` files, without a toolkit.

This is what [MailViewer](https://github.com/alescdb/mailviewer) does before anything is
drawn, split out so that a frontend other than the GTK one can reuse it.

## What is in here

- `message`: parsing of `.eml` (gmime) and `.msg` (msg_parser), attachments, dates.
- `html`: turning the html of a message into something safe to render, with
  [ammonia](https://crates.io/crates/ammonia), plus the content security policy that keeps
  a message from reaching the network.
- `utils`: the few glib helpers the above need.

There is no GTK, no libadwaita and no WebKit. There is still GLib, through `gio` for file
handling and `gmime` for parsing.

## Building

```
cargo build
cargo test
```

Needs `libgmime-3.0-dev` and `libglib2.0-dev`.
