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

/// Connects to a relay URL, subscribes to Nostr events, and stores received events in the database.
async fn listen_to_relay(
    relay_url: &str,
    allowed_event_kinds: &[u64],
    db_pool: SqlitePool,
) -> Result<(), Box<dyn Error>> {
    let url = Url::parse(relay_url)?;
    let (ws_stream, _response) = connect_async(url).await?;
    println!("Connected to relay: {}", relay_url);

    let (mut write, mut read) = ws_stream.split();
    let subscription_id = Uuid::new_v4().to_string();

    // Send subscription request
    let req_message = serde_json::json!(["REQ", subscription_id, { "kinds": allowed_event_kinds }]);
    write.send(Message::Text(req_message.to_string())).await?;
    println!("Subscription sent to relay {}.", relay_url);

    while let Some(message) = read.next().await {
        let message = message?;
        if message.is_text() {
            let text = message.into_text()?;
            let value: Value = serde_json::from_str(&text)?;
            if let Some(arr) = value.as_array() {
                if arr.len() >= 3 && arr[0] == "EVENT" {
                    let event_obj = &arr[2];
                    let event: NostrEvent = serde_json::from_value(event_obj.clone())?;

                    // Process only allowed event kinds
                    if allowed_event_kinds.contains(&event.kind) {
                        println!(
                            "Received event kind {} (ID: {}) from relay {}",
                            event.kind, event.id, relay_url
                        );

                        // Determine storage folder and referenced event (if applicable).
                        let (folder, ref_event) = match event.kind {
                            0 => ("users".to_string(), None), // User metadata
                            1 => {
                                // Note events may contain a reference via tag "e".
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
                                // Reaction events require an "e" tag
                                if let Some(tag) = event
                                    .tags
                                    .iter()
                                    .find(|t| t.get(0).map(|s| s == "e").unwrap_or(false))
                                {
                                    if let Some(ref_event_id) = tag.get(1) {
                                        ("reactions".to_string(), Some(ref_event_id.clone()))
                                    } else {
                                        eprintln!(
                                            "Reaction event {} missing 'e' tag value.",
                                            event.id
                                        );
                                        continue;
                                    }
                                } else {
                                    eprintln!("Reaction event {} has no 'e' tag.", event.id);
                                    continue;
                                }
                            }
                            9734 | 9735 => {
                                // Zap events (optional referenced event)
                                let ref_ev = event
                                    .tags
                                    .iter()
                                    .find(|t| t.get(0).map(|s| s == "e").unwrap_or(false))
                                    .and_then(|tag| tag.get(1).cloned());
                                ("zaps".to_string(), ref_ev)
                            }
                            30023 | 30024 => ("long".to_string(), None), // Long-form events
                            _ => continue,                               // Ignore other event kinds
                        };

                        // Convert to DbEvent structure
                        let db_event = DbEvent {
                            event_id: event.id.clone(),
                            pubkey: event.pubkey.clone(),
                            created_at: event.created_at as i64,
                            kind: event.kind as i64,
                            content: event.content.clone(),
                            sig: event.sig.clone(),
                            tags: serde_json::to_string(&event.tags)?,
                            folder,
                            ref_event,
                        };

                        // Insert event using "INSERT OR IGNORE" to avoid duplicates.
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

/// HTTP endpoint to retrieve a zap event.
async fn get_zap_event(id: web::Path<String>, db_pool: web::Data<SqlitePool>) -> impl Responder {
    query_event("zaps", id.into_inner(), db_pool.get_ref()).await
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

    // Only allow folder listings for replies, reactions, and zaps.
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

/// Handler for querying a single event from a folder using two URL parameters.
/// For example: GET /replies/{ref_event}/{id}
async fn get_event_from_folder(
    path: web::Path<(String, String)>,
    db_pool: web::Data<SqlitePool>,
) -> impl Responder {
    let (folder, identifier) = path.into_inner();
    query_event(&folder, identifier, db_pool.get_ref()).await
}

/// Main entry point of the application.
/// 1. Loads configuration.
/// 2. Creates a SQLite connection pool and ensures the events table exists.
/// 3. Spawns tasks for each relay connection.
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

    // Spawn a task for each relay URL.
    for relay_url in config.relays.urls.clone() {
        let db_pool_clone = db_pool.clone();
        let event_kinds = config.event.kinds.clone();
        let relay_url_clone = relay_url.clone();
        tokio::spawn(async move {
            if let Err(e) = listen_to_relay(&relay_url_clone, &event_kinds, db_pool_clone).await {
                eprintln!("Error on relay {}: {}", relay_url_clone, e);
            }
        });
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
            .route("/zaps/{id}", web::get().to(get_zap_event))
            .route("/long/{id}", web::get().to(get_long_event))
            // Folder listing endpoints (e.g., /replies/{ref_event} and /replies/{ref_event}/{id})
            .route("/replies/{ref_event}", web::get().to(list_folder_events))
            .route(
                "/replies/{ref_event}/{id}",
                web::get().to(get_event_from_folder),
            )
            .route("/reactions/{ref_event}", web::get().to(list_folder_events))
            .route(
                "/reactions/{ref_event}/{id}",
                web::get().to(get_event_from_folder),
            )
            .route("/zaps/{ref_event}", web::get().to(list_folder_events))
            .route(
                "/zaps/{ref_event}/{id}",
                web::get().to(get_event_from_folder),
            )
            // Configuration endpoint
            .route("/config", web::get().to(get_config))
    })
    .bind(&config.server.bind_address)?
    .run()
    .await
}
