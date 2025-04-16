use actix_web::{web, App, HttpResponse, HttpServer};
use serde_json::json;
use sqlx::postgres::PgPool;
use uuid::Uuid;
use chrono::Utc;
use std::fs;
use std::process::Command;
use serde_json::Value;
use std::time::Duration;
use openssl::ssl::{SslAcceptor, SslFiletype, SslMethod};

mod models;
use models::{User, NewUser, AddReferralData};

const XRAY_CONFIG_PATH: &str = "/usr/local/etc/xray/config.json";

fn update_xray_config(uuid: &str) -> Result<(), String> {
    let config_data = fs::read_to_string(XRAY_CONFIG_PATH)
        .map_err(|e| format!("Ошибка чтения конфигурации: {}", e))?;
    let mut config: Value = serde_json::from_str(&config_data)
        .map_err(|e| format!("Ошибка парсинга JSON: {}", e))?;
    if let Some(inbounds) = config["inbounds"].as_array_mut() {
        for inbound in inbounds {
            if inbound["tag"] == "vless-inbound" {
                if let Some(clients) = inbound["settings"]["clients"].as_array_mut() {
                    clients.push(json!({ "id": uuid }));
                }
            }
        }
    }

    fs::write(XRAY_CONFIG_PATH, serde_json::to_string_pretty(&config).unwrap())
        .map_err(|e| format!("Ошибка записи конфигурации: {}", e))?;

    Command::new("systemctl")
        .arg("restart")
        .arg("xray")
        .output()
        .map_err(|e| format!("Ошибка при отправке SIGHUP: {}", e))?;

    Ok(())
}

fn remove_user_from_xray_config(uuid: &str) -> Result<(), String> {
    let config_data = fs::read_to_string(XRAY_CONFIG_PATH)
        .map_err(|e| format!("Ошибка чтения конфигурации: {}", e))?;

    let mut config: Value = serde_json::from_str(&config_data)
        .map_err(|e| format!("Ошибка парсинга JSON: {}", e))?;

    if let Some(inbounds) = config["inbounds"].as_array_mut() {
        for inbound in inbounds {
            if inbound["tag"] == "vless-inbound" {
                if let Some(clients) = inbound["settings"]["clients"].as_array_mut() {
                    clients.retain(|client| client["id"] != uuid);
                }
            }
        }
    }

    fs::write(XRAY_CONFIG_PATH, serde_json::to_string_pretty(&config).unwrap())
        .map_err(|e| format!("Ошибка записи конфигурации: {}", e))?;

    Command::new("systemctl")
        .arg("restart")
        .arg("xray")
        .output()
        .map_err(|e| format!("Ошибка при отправке SIGHUP: {}", e))?;

    Ok(())
}

fn check_user_in_xray_config(uuid: &str) -> bool {
    let config_data = match fs::read_to_string(XRAY_CONFIG_PATH) {
        Ok(data) => data,
        Err(_) => return false,  // Если не удалось прочитать конфиг, считаем, что пользователя нет
    };

    let config: Value = match serde_json::from_str(&config_data) {
        Ok(config) => config,
        Err(_) => return false,  // Если не удалось распарсить JSON, считаем, что пользователя нет
    };

    if let Some(inbounds) = config["inbounds"].as_array() {
        for inbound in inbounds {
            if inbound["tag"] == "vless-inbound" {
                if let Some(clients) = inbound["settings"]["clients"].as_array() {
                    for client in clients {
                        if client["id"] == uuid {
                            return true;  // Пользователь найден в конфиге
                        }
                    }
                }
            }
        }
    }

    false  // Если пользователь не найден
}

async fn cleanup_task(pool: web::Data<PgPool>) {
    let mut interval = tokio::time::interval(Duration::from_secs(1800));

    loop {
        interval.tick().await;

        let expired_users = match sqlx::query!("SELECT uuid FROM users WHERE (subscription_end < NOW() AND is_active = 1) OR is_active = 2")
            .fetch_all(pool.get_ref())
            .await
        {
            Ok(users) => users,
            Err(_) => continue,
        };

        for user in expired_users {
            if let Err(e) = remove_user_from_xray_config(&user.uuid.to_string()) {
                eprintln!("Ошибка удаления пользователя из Xray: {}", e);
                continue;
            }

            let _ = sqlx::query!("UPDATE users SET is_active = 0 WHERE uuid = $1", user.uuid)
                .execute(pool.get_ref())
                .await;
        }
    }
}


async fn extend_subscription(
    pool: web::Data<PgPool>,
    uuid: web::Path<i64>,
    days: web::Json<u32>,
) -> HttpResponse {
    let uuid = uuid.into_inner();

    // Проверяем, существует ли пользователь в конфиге Xray
    let user_exists_in_config = check_user_in_xray_config(&uuid.to_string());

    if !user_exists_in_config {
        if let Err(e) = update_xray_config(&uuid.to_string()) {
            return HttpResponse::InternalServerError().body(format!("Xray конфиг ошибка: {}", e));
        }

        HttpResponse::Ok()
    }
}


#[actix_web::main]
async fn main() -> std::io::Result<()> {
    dotenv::dotenv().ok();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect(&std::env::var("DATABASE_URL").unwrap())
        .await
        .unwrap();

    // Настройка SSL
    let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls())?;
    builder.set_private_key_file("certs/privkey.pem", SslFiletype::PEM)?;
    builder.set_certificate_chain_file("certs/fullchain.pem")?;


    let pool_clone = pool.clone();
    tokio::spawn(async move {
        cleanup_task(web::Data::new(pool_clone)).await
    });

    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .service(
                web::resource("extend/{telegram_id}")
                    .route(web::patch().to(extend_subscription)),
            )
    })
    .bind_openssl("0.0.0.0:443", builder)?
    .run()
    .await
}
