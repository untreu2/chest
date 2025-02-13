use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use config::ConfigError;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::SqlitePool;
use std::error::Error;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url;
use uuid::Uuid;

/// Configuration loaded from `config.toml`
#[derive(Debug, Serialize, Deserialize, Clone)]
struct AppConfig {
    server: ServerConfig,
    relays: RelayConfig,
    event: EventConfig,
    database: DatabaseConfig,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ServerConfig {
    bind_address: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct RelayConfig {
    urls: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct EventConfig {
    kinds: Vec<u64>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct DatabaseConfig {
    /// Path to the SQLite database file (default: "events.db" in chest/chest)
    path: String,
}

/// Nostr event structure
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NostrEvent {
    pub id: String,
    pub pubkey: String,
    pub created_at: u64,
    pub kind: u64,
    pub tags: Vec<Vec<String>>,
    pub content: String,
    pub sig: String,
}

/// Database record structure for events
/// The `tags` field is stored as a JSON string
#[derive(sqlx::FromRow, Debug, Clone, Serialize)]
struct DbEvent {
    event_id: String,
    pubkey: String,
    created_at: i64,
    kind: i64,
    content: String,
    sig: String,
    tags: String,
    folder: String,
    ref_event: Option<String>,
}

/// Opens a connection and subscribes to only a specific event kind.
/// A separate WebSocket connection is created for each relay–event kind pair.
async fn listen_to_relay_for_kind(
    relay_url: &str,
    event_kind: u64,
    db_pool: SqlitePool,
) -> Result<(), Box<dyn Error>> {
    let url = Url::parse(relay_url)?;
    let (ws_stream, _response) = connect_async(url).await?;
    println!(
        "Connected to relay: {} for event kind: {}",
        relay_url, event_kind
    );

    let (mut write, mut read) = ws_stream.split();
    let subscription_id = Uuid::new_v4().to_string();

    // Subscription request: filtering for a single event kind.
    let req_message = serde_json::json!(["REQ", subscription_id, { "kinds": [event_kind] }]);
    write.send(Message::Text(req_message.to_string())).await?;
    println!(
        "Subscription sent to relay {} for kind {}.",
        relay_url, event_kind
    );

    while let Some(message) = read.next().await {
        let message = message?;
        if message.is_text() {
            let text = message.into_text()?;
            let value: Value = serde_json::from_str(&text)?;
            if let Some(arr) = value.as_array() {
                // Relay message format: ["EVENT", subscription_id, event_obj]
                if arr.len() >= 3 && arr[0] == "EVENT" {
                    let event_obj = &arr[2];
                    let event: NostrEvent = serde_json::from_value(event_obj.clone())?;
                    // Since the subscription already filters by event kind, no additional check is required.

                    println!(
                        "Received event kind {} (ID: {}) from relay {}",
                        event.kind, event.id, relay_url
                    );

                    // Determine storage folder and referenced event based on event kind.
                    let (folder, ref_event) = match event.kind {
                        0 => ("users".to_string(), None), // User metadata
                        1 => {
                            // Note events: may contain an "e" tag as reference.
                            if let Some(tag) = event
                                .tags
                                .iter()
                                .find(|t| t.get(0).map(|s| s == "e").unwrap_or(false))
                            {
                                if let Some(ref_event_id) = tag.get(1) {
                                    ("replies".to_string(), Some(ref_event_id.clone()))
                                } else {
                                    ("notes".to_string(), None)
                                }
                            } else {
                                ("notes".to_string(), None)
                            }
                        }
                        7 => {
                            // Reaction events: must contain an "e" tag.
                            if let Some(tag) = event
                                .tags
                                .iter()
                                .find(|t| t.get(0).map(|s| s == "e").unwrap_or(false))
                            {
                                if let Some(ref_event_id) = tag.get(1) {
                                    ("reactions".to_string(), Some(ref_event_id.clone()))
                                } else {
                                    eprintln!("Reaction event {} missing 'e' tag value.", event.id);
                                    continue;
                                }
                            } else {
                                eprintln!("Reaction event {} has no 'e' tag.", event.id);
                                continue;
                            }
                        }
                        9734 | 9735 => {
                            // Zap events
                            let ref_ev = event
                                .tags
                                .iter()
                                .find(|t| t.get(0).map(|s| s == "e").unwrap_or(false))
                                .and_then(|tag| tag.get(1).cloned());
                            ("zaps".to_string(), ref_ev)
                        }
                        30023 | 30024 => ("long".to_string(), None), // Long-form events
                        _ => continue,                               // Ignore other event types.
                    };

                    // Convert to DbEvent structure.
                    let db_event = DbEvent {
                        event_id: event.id.clone(),
                        pubkey: event.pubkey.clone(),
                        created_at: event.created_at as i64,
                        kind: event.kind as i64,
                        content: event.content.clone(),
                        sig: event.sig.clone(),
                        tags: serde_json::to_string(&event.tags)?,
                        folder,
                        ref_event: ref_event.clone(),
                    };

                    // Insert event using "INSERT OR IGNORE" to prevent duplicates.
                    let query = r#"
                        INSERT OR IGNORE INTO events (
                            event_id, pubkey, created_at, kind, content, sig, tags, folder, ref_event
                        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                    "#;
                    match sqlx::query(query)
                        .bind(&db_event.event_id)
                        .bind(&db_event.pubkey)
                        .bind(db_event.created_at)
                        .bind(db_event.kind)
                        .bind(&db_event.content)
                        .bind(&db_event.sig)
                        .bind(&db_event.tags)
                        .bind(&db_event.folder)
                        .bind(&db_event.ref_event)
                        .execute(&db_pool)
                        .await
                    {
                        Ok(_) => println!("Event {} saved to database.", event.id),
                        Err(e) => eprintln!("Error inserting event {}: {:?}", event.id, e),
                    }

                    // Dynamic subscription: If the event is a note (kind 1) and it has not been subscribed to yet,
                    // spawn a new subscription task to listen for reaction, zap, and reply events for this note.
                    if event.kind == 1 {
                        let note_event_id = event.id.clone();
                        let relay_url_clone = relay_url.to_string();
                        let db_pool_clone = db_pool.clone();
                        tokio::spawn(async move {
                            if let Err(e) = dynamic_listen_to_relay_for_note(
                                &relay_url_clone,
                                note_event_id,
                                db_pool_clone,
                            )
                            .await
                            {
                                eprintln!(
                                    "Error in dynamic subscription on relay {}: {}",
                                    relay_url_clone, e
                                );
                            }
                        });
                    }

                    // Dynamic subscription: If the event is a long-form event (kind 30023 or 30024),
                    // spawn a new subscription task to listen for reaction, zap, and reply events for this long content.
                    if event.kind == 30023 || event.kind == 30024 {
                        let long_event_id = event.id.clone();
                        let relay_url_clone = relay_url.to_string();
                        let db_pool_clone = db_pool.clone();
                        tokio::spawn(async move {
                            if let Err(e) = dynamic_listen_to_relay_for_long(
                                &relay_url_clone,
                                long_event_id,
                                db_pool_clone,
                            )
                            .await
                            {
                                eprintln!(
                                    "Error in dynamic subscription on relay {}: {}",
                                    relay_url_clone, e
                                );
                            }
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

/// Dynamic subscription function that connects to a relay and listens for reaction, zap,
/// and reply events that reference a given note event ID.
async fn dynamic_listen_to_relay_for_note(
    relay_url: &str,
    note_event_id: String,
    db_pool: SqlitePool,
) -> Result<(), Box<dyn Error>> {
    let url = Url::parse(relay_url)?;
    let (ws_stream, _response) = connect_async(url).await?;
    println!(
        "Dynamic subscription started on relay {} for note ID {}",
        relay_url, note_event_id
    );

    let (mut write, mut read) = ws_stream.split();
    let subscription_id = Uuid::new_v4().to_string();

    // Subscription request: filter for reaction (7) and zap (9734, 9735) events referencing the note.
    let req_message = serde_json::json!([
        "REQ",
        subscription_id,
        { "kinds": [7, 9734, 9735], "#e": [note_event_id] }
    ]);
    write.send(Message::Text(req_message.to_string())).await?;
    println!(
        "Dynamic subscription request sent to relay {} for note ID {}.",
        relay_url, note_event_id
    );

    while let Some(message) = read.next().await {
        let message = message?;
        if message.is_text() {
            let text = message.into_text()?;
            let value: Value = serde_json::from_str(&text)?;
            if let Some(arr) = value.as_array() {
                // Relay message format: ["EVENT", subscription_id, event_obj]
                if arr.len() >= 3 && arr[0] == "EVENT" {
                    let event_obj = &arr[2];
                    let event: NostrEvent = serde_json::from_value(event_obj.clone())?;
                    println!(
                        "Dynamic subscription received event kind {} (ID: {}) from relay {}",
                        event.kind, event.id, relay_url
                    );

                    // Determine storage folder based on event kind.
                    let folder = match event.kind {
                        7 => "reactions".to_string(),
                        9734 | 9735 => "zaps".to_string(),
                        _ => continue, // Ignore other event types.
                    };

                    // For dynamic subscriptions we assume the reference event is the note_event_id.
                    let db_event = DbEvent {
                        event_id: event.id.clone(),
                        pubkey: event.pubkey.clone(),
                        created_at: event.created_at as i64,
                        kind: event.kind as i64,
                        content: event.content.clone(),
                        sig: event.sig.clone(),
                        tags: serde_json::to_string(&event.tags)?,
                        folder,
                        ref_event: Some(note_event_id.clone()),
                    };

                    // Insert event using "INSERT OR IGNORE" to prevent duplicates.
                    let query = r#"
                        INSERT OR IGNORE INTO events (
                            event_id, pubkey, created_at, kind, content, sig, tags, folder, ref_event
                        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                    "#;
                    match sqlx::query(query)
                        .bind(&db_event.event_id)
                        .bind(&db_event.pubkey)
                        .bind(db_event.created_at)
                        .bind(db_event.kind)
                        .bind(&db_event.content)
                        .bind(&db_event.sig)
                        .bind(&db_event.tags)
                        .bind(&db_event.folder)
                        .bind(&db_event.ref_event)
                        .execute(&db_pool)
                        .await
                    {
                        Ok(_) => println!("Dynamic event {} saved to database.", event.id),
                        Err(e) => eprintln!("Error inserting dynamic event {}: {:?}", event.id, e),
                    }
                }
            }
        }
    }
    Ok(())
}

/// Dynamic subscription function that connects to a relay and listens for reaction, zap,
/// and reply events that reference a given long-form event ID.
async fn dynamic_listen_to_relay_for_long(
    relay_url: &str,
    long_event_id: String,
    db_pool: SqlitePool,
) -> Result<(), Box<dyn Error>> {
    let url = Url::parse(relay_url)?;
    let (ws_stream, _response) = connect_async(url).await?;
    println!(
        "Dynamic subscription started on relay {} for long content ID {}",
        relay_url, long_event_id
    );

    let (mut write, mut read) = ws_stream.split();
    let subscription_id = Uuid::new_v4().to_string();

    // Subscription request: filter for reaction (7) and zap (9734, 9735) events referencing the long content.
    let req_message = serde_json::json!([
        "REQ",
        subscription_id,
        { "kinds": [7, 9734, 9735], "#e": [long_event_id] }
    ]);
    write.send(Message::Text(req_message.to_string())).await?;
    println!(
        "Dynamic subscription request sent to relay {} for long content ID {}.",
        relay_url, long_event_id
    );

    while let Some(message) = read.next().await {
        let message = message?;
        if message.is_text() {
            let text = message.into_text()?;
            let value: Value = serde_json::from_str(&text)?;
            if let Some(arr) = value.as_array() {
                // Relay message format: ["EVENT", subscription_id, event_obj]
                if arr.len() >= 3 && arr[0] == "EVENT" {
                    let event_obj = &arr[2];
                    let event: NostrEvent = serde_json::from_value(event_obj.clone())?;
                    println!(
                        "Dynamic subscription received event kind {} (ID: {}) from relay {}",
                        event.kind, event.id, relay_url
                    );

                    // Determine storage folder based on event kind.
                    let folder = match event.kind {
                        7 => "reactions".to_string(),
                        9734 | 9735 => "zaps".to_string(),
                        _ => continue, // Ignore other event types.
                    };

                    // For dynamic subscriptions we assume the reference event is the long_event_id.
                    let db_event = DbEvent {
                        event_id: event.id.clone(),
                        pubkey: event.pubkey.clone(),
                        created_at: event.created_at as i64,
                        kind: event.kind as i64,
                        content: event.content.clone(),
                        sig: event.sig.clone(),
                        tags: serde_json::to_string(&event.tags)?,
                        folder,
                        ref_event: Some(long_event_id.clone()),
                    };

                    // Insert event using "INSERT OR IGNORE" to prevent duplicates.
                    let query = r#"
                        INSERT OR IGNORE INTO events (
                            event_id, pubkey, created_at, kind, content, sig, tags, folder, ref_event
                        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                    "#;
                    match sqlx::query(query)
                        .bind(&db_event.event_id)
                        .bind(&db_event.pubkey)
                        .bind(db_event.created_at)
                        .bind(db_event.kind)
                        .bind(&db_event.content)
                        .bind(&db_event.sig)
                        .bind(&db_event.tags)
                        .bind(&db_event.folder)
                        .bind(&db_event.ref_event)
                        .execute(&db_pool)
                        .await
                    {
                        Ok(_) => println!("Dynamic event {} saved to database.", event.id),
                        Err(e) => eprintln!("Error inserting dynamic event {}: {:?}", event.id, e),
                    }
                }
            }
        }
    }
    Ok(())
}

/// Query a single event from the database based on folder and identifier.
async fn query_event(folder: &str, identifier: String, db_pool: &SqlitePool) -> HttpResponse {
    let query = if folder == "users" {
        "SELECT event_id, pubkey, created_at, kind, content, sig, tags, folder, ref_event
         FROM events WHERE folder = ? AND pubkey = ?"
    } else {
        "SELECT event_id, pubkey, created_at, kind, content, sig, tags, folder, ref_event
         FROM events WHERE folder = ? AND event_id = ?"
    };

    match sqlx::query_as::<_, DbEvent>(query)
        .bind(folder)
        .bind(&identifier)
        .fetch_optional(db_pool)
        .await
    {
        Ok(Some(event)) => HttpResponse::Ok().json(event),
        Ok(None) => HttpResponse::NotFound().body("Event not found"),
        Err(e) => {
            eprintln!("Database query error: {:?}", e);
            HttpResponse::InternalServerError().body("Internal error")
        }
    }
}

/// HTTP endpoint to retrieve a user event.
async fn get_user_event(id: web::Path<String>, db_pool: web::Data<SqlitePool>) -> impl Responder {
    query_event("users", id.into_inner(), db_pool.get_ref()).await
}

/// HTTP endpoint to retrieve a note event.
async fn get_note_event(id: web::Path<String>, db_pool: web::Data<SqlitePool>) -> impl Responder {
    query_event("notes", id.into_inner(), db_pool.get_ref()).await
}

/// HTTP endpoint to retrieve a long-form event.
async fn get_long_event(id: web::Path<String>, db_pool: web::Data<SqlitePool>) -> impl Responder {
    query_event("long", id.into_inner(), db_pool.get_ref()).await
}

/// Endpoint for listing events in a folder (e.g., replies, reactions, or zaps) based on a reference event.
async fn list_folder_events(
    path: web::Path<(String, String)>,
    db_pool: web::Data<SqlitePool>,
) -> impl Responder {
    let (folder, ref_event) = path.into_inner();

    // Only allowed folder listings for replies, reactions, and zaps.
    let allowed_folders = ["replies", "reactions", "zaps"];
    if !allowed_folders.contains(&folder.as_str()) {
        return HttpResponse::BadRequest().body("Invalid folder name");
    }

    let query = r#"
        SELECT event_id, pubkey, created_at, kind, content, sig, tags, folder, ref_event
        FROM events
        WHERE folder = ? AND ref_event = ?
    "#;

    match sqlx::query_as::<_, DbEvent>(query)
        .bind(&folder)
        .bind(&ref_event)
        .fetch_all(db_pool.get_ref())
        .await
    {
        Ok(events) => HttpResponse::Ok().json(events),
        Err(e) => {
            eprintln!("Database query error: {:?}", e);
            HttpResponse::InternalServerError().body("Internal error")
        }
    }
}

/// HTTP endpoint to retrieve the application configuration.
async fn get_config(config: web::Data<AppConfig>) -> impl Responder {
    HttpResponse::Ok().json(config.get_ref())
}

/// Loads configuration from `config.toml`
fn load_config() -> Result<AppConfig, ConfigError> {
    let settings = config::Config::builder()
        .add_source(config::File::with_name("config"))
        .build()?;
    settings.try_deserialize::<AppConfig>()
}

/// Lists all note events for a specific user based on their pubkey.
async fn list_notes_by_pubkey(
    path: web::Path<String>,
    db_pool: web::Data<SqlitePool>,
) -> impl Responder {
    let pubkey = path.into_inner();
    let query = r#"
        SELECT event_id, pubkey, created_at, kind, content, sig, tags, folder, ref_event
        FROM events
        WHERE folder = 'notes' AND pubkey = ?
    "#;

    match sqlx::query_as::<_, DbEvent>(query)
        .bind(&pubkey)
        .fetch_all(db_pool.get_ref())
        .await
    {
        Ok(events) => HttpResponse::Ok().json(events),
        Err(e) => {
            eprintln!("Database query error: {:?}", e);
            HttpResponse::InternalServerError().body("Internal error")
        }
    }
}

/// Main entry point of the application.
/// 1. Loads configuration.
/// 2. Creates a SQLite connection pool and ensures the events table exists.
/// 3. Spawns tasks for each relay–event kind combination.
/// 4. Starts the HTTP server.
#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // Load configuration
    let config: AppConfig = match load_config() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Failed to read configuration: {}", e);
            std::process::exit(1);
        }
    };
    println!("Loaded configuration: {:?}", config);

    // Create the SQLite database connection pool.
    let db_pool = SqlitePool::connect(&config.database.path)
        .await
        .expect("Failed to connect to the database");

    // Create the events table if it does not exist.
    let create_table_query = r#"
        CREATE TABLE IF NOT EXISTS events (
            event_id TEXT PRIMARY KEY,
            pubkey TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            kind INTEGER NOT NULL,
            content TEXT NOT NULL,
            sig TEXT NOT NULL,
            tags TEXT NOT NULL,
            folder TEXT NOT NULL,
            ref_event TEXT
        );
    "#;
    if let Err(e) = sqlx::query(create_table_query).execute(&db_pool).await {
        eprintln!("Failed to create table: {:?}", e);
        std::process::exit(1);
    }

    // Global subscriptions: only subscribe for note events (kind 1) and long-form events (30023, 30024).
    let global_event_kinds = vec![1, 30023, 30024];

    // Spawn a task for each relay URL and for each global event kind.
    for relay_url in config.relays.urls.clone() {
        for event_kind in global_event_kinds.clone() {
            let db_pool_clone = db_pool.clone();
            let relay_url_clone = relay_url.clone();
            tokio::spawn(async move {
                if let Err(e) =
                    listen_to_relay_for_kind(&relay_url_clone, event_kind, db_pool_clone).await
                {
                    eprintln!(
                        "Error on relay {} for kind {}: {}",
                        relay_url_clone, event_kind, e
                    );
                }
            });
        }
    }

    // Share configuration and database pool with HTTP server.
    let config_data = web::Data::new(config.clone());
    let db_pool_data = web::Data::new(db_pool);

    HttpServer::new(move || {
        App::new()
            .app_data(config_data.clone())
            .app_data(db_pool_data.clone())
            // Single event endpoints
            .route("/users/{id}", web::get().to(get_user_event))
            .route("/notes/{id}", web::get().to(get_note_event))
            .route("/long/{id}", web::get().to(get_long_event))
            // Folder listing endpoints
            .route("/replies/{ref_event}", web::get().to(list_folder_events))
            .route("/reactions/{ref_event}", web::get().to(list_folder_events))
            .route("/zaps/{ref_event}", web::get().to(list_folder_events))
            // List all notes for a specific user by pubkey.
            .route(
                "/notes/pubkey/{pubkey}",
                web::get().to(list_notes_by_pubkey),
            )
            // Configuration endpoint
            .route("/config", web::get().to(get_config))
    })
    .bind(&config.server.bind_address)?
    .run()
    .await
}
