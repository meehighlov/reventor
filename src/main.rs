use teloxide::{prelude::*, utils::command::BotCommands};
use teloxide::RequestError;
use dotenv::dotenv;
use std::env;
use regex::Regex;
use chrono::{NaiveDateTime, Datelike, Timelike};
use rusqlite::{Connection, params, OptionalExtension};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug)]
struct DatabaseError(rusqlite::Error);

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

#[derive(Debug)]
struct UserEvent {
    text: String,
    event_time: String,
}

#[derive(Debug)]
struct NotificationEvent {
    telegram_id: i64,
    text: String,
    event_time: String,
}

fn init_db(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            telegram_id INTEGER NOT NULL UNIQUE,
            username TEXT,
            created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
        [],
    )?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL,
            text TEXT NOT NULL,
            event_time DATETIME NOT NULL,
            created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY(user_id) REFERENCES users(id)
        )",
        [],
    )?;

    Ok(())
}

fn ensure_user_exists(conn: &Connection, telegram_id: i64, username: Option<String>) -> Result<i64, rusqlite::Error> {
    let existing_id: Option<i64> = conn.query_row(
        "SELECT id FROM users WHERE telegram_id = ?",
        params![telegram_id],
        |row| row.get(0),
    ).optional()?;

    match existing_id {
        Some(id) => Ok(id),
        None => {
            conn.execute(
                "INSERT INTO users (telegram_id, username) VALUES (?, ?)",
                params![telegram_id, username],
            )?;
            Ok(conn.last_insert_rowid())
        }
    }
}

fn save_event(conn: &Connection, user_id: i64, event: &Event) -> Result<(), rusqlite::Error> {
    let event_time = match &event.date {
        Some(date) => {
            if date.matches('.').count() == 1 {
                let current_year = chrono::Local::now().year();
                format!("{}.{} {}", date, current_year, event.time)
            } else {
                format!("{} {}", date, event.time)
            }
        },
        None => {
            let today = chrono::Local::now().format("%d.%m.%Y").to_string();
            format!("{} {}", today, event.time)
        }
    };

    println!("Parsing datetime: {}", event_time);

    // Преобразуем в нужный формат без секунд
    let event_datetime = NaiveDateTime::parse_from_str(&format!("{}:00", event_time), "%d.%m.%Y %H:%M:%S")
        .unwrap_or_else(|_| panic!("Failed to parse date: {}", event_time))
        .format("%d.%m.%Y %H:%M")
        .to_string();

    conn.execute(
        "INSERT INTO events (user_id, text, event_time) VALUES (?, ?, ?)",
        params![user_id, event.text, event_datetime],
    )?;

    Ok(())
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

fn get_user_events(conn: &Connection, telegram_id: i64) -> Result<Vec<UserEvent>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT e.text, e.event_time 
         FROM events e 
         JOIN users u ON e.user_id = u.id 
         WHERE u.telegram_id = ? 
         ORDER BY e.event_time"
    )?;

    let events = stmt.query_map(params![telegram_id], |row| {
        Ok(UserEvent {
            text: row.get(0)?,
            event_time: row.get(1)?,
        })
    })?
    .collect::<Result<Vec<_>, _>>()?;

    Ok(events)
}

fn get_due_events(conn: &Connection) -> Result<Vec<NotificationEvent>, rusqlite::Error> {
    let now = chrono::Local::now().format("%d.%m.%Y %H:%M").to_string();
    println!("Checking events at: {}", now);

    let mut stmt = conn.prepare(
        "SELECT u.telegram_id, e.text, e.event_time 
         FROM events e 
         JOIN users u ON e.user_id = u.id 
         WHERE e.event_time = ?"
    )?;

    let events = stmt.query_map(params![now], |row| {
        let event_time: String = row.get(2)?;
        let telegram_id: i64 = row.get(0)?;
        println!("Found matching event: time={}, telegram_id={}", event_time, telegram_id);
        
        Ok(NotificationEvent {
            telegram_id,
            text: row.get(1)?,
            event_time,
        })
    })?
    .collect::<Result<Vec<_>, _>>()?;

    println!("Total events found for time {}: {}", now, events.len());
    for event in &events {
        println!("Event details: {:?}", event);
    }

    Ok(events)
}

fn mark_event_sent(conn: &Connection, telegram_id: i64, event_time: &str) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE events SET event_time = 'done' 
         WHERE user_id IN (SELECT id FROM users WHERE telegram_id = ?) 
         AND event_time = ?",
        params![telegram_id, event_time],
    )?;
    Ok(())
}

#[tokio::main]
async fn main() {
    dotenv().ok();
    pretty_env_logger::init();
    log::info!("Starting reminder bot...");

    let token = env::var("TELOXIDE_TOKEN").expect("TELOXIDE_TOKEN не найден в .env файле");
    let bot = Bot::new(token);

    let conn = Connection::open("reventor.db").expect("Failed to open database");
    init_db(&conn).expect("Failed to initialize database");
    let db = Arc::new(Mutex::new(conn));

    let bot_for_notifications = bot.clone();
    let db_for_notifications = db.clone();

    tokio::spawn(async move {
        loop {
            let conn = db_for_notifications.lock().await;
            println!("Checking for due events...");
            
            if let Ok(events) = get_due_events(&conn) {
                println!("Found {} due events", events.len());
                for event in events {
                    println!("Sending notification for event: {:?}", event);
                    let _ = bot_for_notifications
                        .send_message(
                            ChatId(event.telegram_id),
                            format!("🔔 Напоминание!\n{}\nВремя: {}", event.text, event.event_time)
                        )
                        .await;
                    
                    // Очищаем event_time после отправки уведомления
                    let _ = mark_event_sent(&conn, event.telegram_id, &event.event_time);
                }
            }
            drop(conn);
            
            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
        }
    });

    teloxide::repl(bot, move |bot: Bot, msg: Message| {
        let db = db.clone();
        async move {
            if let Some(text) = msg.text() {
                if text == "/events" {
                    let conn = db.lock().await;
                    let events = get_user_events(&conn, msg.from().unwrap().id.0 as i64)
                        .map_err(|e| DatabaseError(e))?;

                    if events.is_empty() {
                        bot.send_message(msg.chat.id, "У вас пока нет запланированных событий").await?;
                    } else {
                        let events_text = events
                            .iter()
                            .enumerate()
                            .map(|(i, e)| format!("{}. {} - {}", i + 1, e.event_time, e.text))
                            .collect::<Vec<_>>()
                            .join("\n");
                        
                        bot.send_message(msg.chat.id, format!("Ваши события:\n{}", events_text)).await?;
                    }
                } else if let Some(event) = parse_event(text) {
                    let conn = db.lock().await;
                    let user_id = ensure_user_exists(
                        &conn,
                        msg.from().unwrap().id.0 as i64,
                        msg.from().unwrap().username.clone()
                    ).map_err(|e| DatabaseError(e))?;

                    save_event(&conn, user_id, &event)
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
