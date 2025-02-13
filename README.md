# chest: A database server written in Rust to store Nostr events

## Configuration
**chest** uses `config.toml` file to define server settings, relay connections, event types to track, and the database path.

```toml
[server]
bind_address = "127.0.0.1:8080"

[relays]
urls = [
  "wss://bitcoiner.social",
  "wss://nos.lol",
  "wss://nostr.oxtr.dev",
  "wss://relay.damus.io",
  "wss://relay.primal.net",
  "wss://vitor.nostr1.com"
]

[event]
kinds = [0, 1, 7, 9734, 9735, 30023, 30024]

[database]
path = "events.db"
```
