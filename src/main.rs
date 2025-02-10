use teloxide::{prelude::*, utils::command::BotCommands};
use teloxide::error_handlers::OnError;
use teloxide::types::Update;
use teloxide::adaptors::DefaultParseMode;
use teloxide::payloads::SendMessage;
use teloxide::errors::RequestError;
use dotenv::dotenv;
use std::env;
use regex::Regex;
use chrono::NaiveDateTime;
use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};
use std::io::{Error as IoError, ErrorKind};
use std::fs;

#[derive(Debug)]
struct DatabaseError(sqlx::Error);

impl From<DatabaseError> for RequestError {
    fn from(err: DatabaseError) -> Self {
        RequestError::Api(teloxide::ApiError::Unknown(err.0.to_string()))
    }
}

#[derive(Debug)]
struct Event {
    text: String,
    time: String,
    date: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
struct DbUser {
    id: i64,
    telegram_id: i64,
    username: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, sqlx::FromRow)]
struct DbEvent {
    id: i64,
    user_id: i64,
    text: String,
    event_time: chrono::DateTime<chrono::Utc>,
    created_at: chrono::DateTime<chrono::Utc>,
}

fn parse_event(text: &str) -> Option<Event> {
    let re = Regex::new(r"@(?:(\d{2}\.\d{2}(?:\.\d{4})?)\s+)?(\d{2}:\d{2})").unwrap();
    
    if let Some(captures) = re.captures(text) {
        let time = captures.get(2).unwrap().as_str().to_string();
        let date = captures.get(1).map(|m| m.as_str().to_string());
        
        Some(Event {
            text: text.to_string(),
            time,
            date,
        })
    } else {
        None
    }
}

async fn init_db(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            telegram_id INTEGER NOT NULL UNIQUE,
            username TEXT,
            created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
        )"
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL,
            text TEXT NOT NULL,
            event_time DATETIME NOT NULL,
            created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY(user_id) REFERENCES users(id)
        )"
    )
    .execute(pool)
    .await?;

    Ok(())
}

async fn ensure_user_exists(pool: &SqlitePool, telegram_id: i64, username: Option<String>) -> Result<i64, sqlx::Error> {
    // Пробуем найти существующего пользователя
    let user = sqlx::query_as::<_, DbUser>(
        "SELECT * FROM users WHERE telegram_id = ?"
    )
    .bind(telegram_id)
    .fetch_optional(pool)
    .await?;

    match user {
        Some(user) => Ok(user.id),
        None => {
            // Создаем нового пользователя
            let result = sqlx::query(
                "INSERT INTO users (telegram_id, username) VALUES (?, ?)"
            )
            .bind(telegram_id)
            .bind(username)
            .execute(pool)
            .await?;

            Ok(result.last_insert_rowid())
        }
    }
}

async fn save_event(pool: &SqlitePool, user_id: i64, event: &Event) -> Result<(), sqlx::Error> {
    let event_time = match &event.date {
        Some(date) => {
            format!("{} {}", date, event.time)
        },
        None => {
            let today = chrono::Local::now().format("%d.%m.%Y").to_string();
            format!("{} {}", today, event.time)
        }
    };

    let event_time = chrono::NaiveDateTime::parse_from_str(
        &format!("{} {}", event_time, "+00:00"),
        "%d.%m.%Y %H:%M %z"
    )
    .unwrap()
    .and_utc();

    sqlx::query(
        "INSERT INTO events (user_id, text, event_time) VALUES (?, ?, ?)"
    )
    .bind(user_id)
    .bind(&event.text)
    .bind(event_time)
    .execute(pool)
    .await?;

    Ok(())
}

#[tokio::main]
async fn main() {
    dotenv().ok();
    pretty_env_logger::init();
    log::info!("Starting reminder bot...");

    let token = env::var("TELOXIDE_TOKEN").expect("TELOXIDE_TOKEN не найден в .env файле");
    let bot = Bot::new(token);

    let database_url = env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:./data/data.db".to_string());
    
    // Добавляем эти строки перед подключением к БД
    std::fs::create_dir_all("./data").expect("Failed to create data directory");
    
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await
        .expect("Could not connect to database");

    init_db(&pool).await.expect("Failed to initialize database");

    teloxide::repl(bot, move |bot: Bot, msg: Message| {
        let pool = pool.clone();
        async move {
            if let Some(text) = msg.text() {
                if let Some(event) = parse_event(text) {
                    let user_id = ensure_user_exists(
                        &pool,
                        msg.from().unwrap().id.0 as i64,
                        msg.from().unwrap().username.clone()
                    ).await.map_err(|e| DatabaseError(e))?;

                    save_event(&pool, user_id, &event).await
                        .map_err(|e| DatabaseError(e))?;

                    let response = match event.date {
                        Some(date) => format!("Сохранено событие на {} в {}\nТекст события: {}", 
                            date, event.time, event.text),
                        None => format!("Сохранено событие на сегодня в {}\nТекст события: {}", 
                            event.time, event.text),
                    };
                    bot.send_message(msg.chat.id, response).await?;
                } else {
                    bot.send_message(msg.chat.id, "Привет! Чтобы создать событие, используйте форматы:\n\
                        @ЧЧ:ММ - событие на сегодня\n\
                        @ДД.ММ ЧЧ:ММ - событие на конкретную дату\n\
                        @ДД.ММ.ГГГГ ЧЧ:ММ - событие на конкретную дату с годом").await?;
                }
            }
            Ok(())
        }
    })
    .await;
}
