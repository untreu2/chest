use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use config::ConfigError;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::error::Error;
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream};
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
    /// Path to the SQLite database file (default: chest/events.db)
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

/// WebSocket connection holder
#[derive(Debug)]
struct WSConnection {
    write: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<MaybeTlsStream<TcpStream>>,
        Message,
    >,
    read: Option<
        futures_util::stream::SplitStream<
            tokio_tungstenite::WebSocketStream<MaybeTlsStream<TcpStream>>,
        >,
    >,
}

/// Manages a single WebSocket connection per relay
#[derive(Debug)]
struct WebSocketManager {
    connections: HashMap<String, WSConnection>,
}

impl WebSocketManager {
    /// Creates a new manager and attempts to connect to all provided relay URLs
    async fn new(relay_urls: &[String]) -> Self {
        let mut connections = HashMap::new();
        for relay_url in relay_urls {
            if let Ok(conn) = Self::connect(relay_url).await {
                connections.insert(relay_url.clone(), conn);
                println!("Connected to relay: {}", relay_url);
            } else {
                eprintln!("Failed to connect to relay: {}", relay_url);
            }
        }
        Self { connections }
    }

    /// Establishes a WebSocket connection to a single relay
    async fn connect(relay_url: &str) -> Result<WSConnection, Box<dyn Error>> {
        let url = Url::parse(relay_url)?;
        let (ws_stream, _) = connect_async(url).await?;
        let (write, read) = ws_stream.split();
        Ok(WSConnection {
            write,
            read: Some(read),
        })
    }

    /// Sends a subscription REQ to a relay, if connected
    async fn add_subscription(
        &mut self,
        relay_url: &str,
        req_message: Value,
    ) -> Result<(), Box<dyn Error>> {
        if let Some(conn) = self.connections.get_mut(relay_url) {
            conn.write
                .send(Message::Text(req_message.to_string()))
                .await?;
            println!(
                "Subscription added on relay: {} with request: {}",
                relay_url, req_message
            );
        } else {
            eprintln!("No connection found for relay: {}", relay_url);
        }
        Ok(())
    }

    /// Listens to messages from all relay connections
    async fn listen(&mut self) {
        for (relay_url, conn) in self.connections.iter_mut() {
            if let Some(mut read) = conn.read.take() {
                let relay_url = relay_url.clone();
                tokio::spawn(async move {
                    while let Some(message) = read.next().await {
                        match message {
                            Ok(Message::Text(text)) => {
                                println!("Message received from {}: {}", relay_url, text);
                            }
                            Ok(Message::Close(_)) => {
                                println!("Connection closed for relay: {}", relay_url);
                                break;
                            }
                            Err(e) => {
                                eprintln!("Error receiving message from {}: {}", relay_url, e);
                                break;
                            }
                            _ => {}
                        }
                    }
                });
            }
        }
    }
}

/// Loads configuration from `config.toml`
fn load_config() -> Result<AppConfig, ConfigError> {
    let settings = config::Config::builder()
        .add_source(config::File::with_name("config"))
        .build()?;
    settings.try_deserialize::<AppConfig>()
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
/// 3. Subscribes to certain event kinds on all relays.
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

    // Event kinds we want to subscribe to globally:
    let global_event_kinds = vec![1, 30023, 30024];

    // Create a WebSocketManager for all relays.
    let mut ws_manager = WebSocketManager::new(&config.relays.urls).await;

    // Add subscriptions for each relay for each global event kind.
    for relay_url in &config.relays.urls {
        for event_kind in &global_event_kinds {
            let subscription_id = Uuid::new_v4().to_string();
            let req_message =
                serde_json::json!(["REQ", subscription_id, { "kinds": [event_kind] }]);
            if let Err(e) = ws_manager.add_subscription(relay_url, req_message).await {
                eprintln!(
                    "Error adding subscription on relay {} for kind {}: {}",
                    relay_url, event_kind, e
                );
            }
        }
    }

    // Start listening to messages on all WebSocket connections.
    ws_manager.listen().await;

    // Share configuration and database pool with the HTTP server.
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
