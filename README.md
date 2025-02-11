# chest: A database server written in Rust to store Nostr events

## Configuration
The application uses a `config.toml` file to define server settings, relay connections, event types to track, and the database path.

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

## API Endpoints
The API provides HTTP endpoints to retrieve event data stored in the database.

### Get a User Event
**Request:**
```bash
curl -X GET http://127.0.0.1:8080/users/{hex_id}
```
**Response:**
Returns the stored user metadata event if found.

### Get a Note Event
**Request:**
```bash
curl -X GET http://127.0.0.1:8080/notes/{hex_id}
```
**Response:**
Returns the stored note event if found.

### Get a Zap Event
**Request:**
```bash
curl -X GET http://127.0.0.1:8080/zaps/{hex_id}
```
**Response:**
Returns the stored zap event if found.

### Get a Long-form Event
**Request:**
```bash
curl -X GET http://127.0.0.1:8080/long/hex_id}
```
**Response:**
Returns the stored long-form event if found.

### List Events in a Folder
#### Replies
**Request:**
```bash
curl -X GET http://127.0.0.1:8080/replies/{ref_event_hex_id}
```
**Response:**
Returns all stored events in the `replies` folder linked to `ref_event`.

#### Zaps
**Request:**
```bash
curl -X GET http://127.0.0.1:8080/zaps/{ref_event_hex_id}
```
**Response:**
Returns all stored zap events linked to `ref_event`.

#### Reactions
**Request:**
```bash
curl -X GET http://127.0.0.1:8080/reactions/{ref_event_hex_id}
```
**Response:**
Returns all stored reaction events linked to `ref_event`.

### Get API Configuration
**Request:**
```bash
curl -X GET http://127.0.0.1:8080/config
```
**Response:**
Returns the current API configuration settings.

## How It Works
1. The API loads configuration settings from `config.toml`.
2. It connects to defined Nostr relay servers via WebSocket.
3. It listens for and filters incoming events based on predefined kinds.
4. Valid events are stored in a SQLite database.
5. HTTP endpoints allow users to retrieve stored events based on event ID.

## Running the API (chest/chest)
```bash
cargo build --release
```
```bash
cargo run
```
