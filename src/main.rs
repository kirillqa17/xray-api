use actix_web::{web, App, HttpResponse, HttpServer};
use serde_json::json;
use sqlx::postgres::PgPool;
use std::fs;
use std::process::Command;
use serde_json::Value;
use std::time::Duration;
use openssl::ssl::{SslAcceptor, SslFiletype, SslMethod};

const XRAY_CONFIG_PATH: &str = "/usr/local/etc/xray/config.json";

fn update_xray_config(uuid: &str, conn_limit: i32) -> Result<(), String> {
    let config_data = fs::read_to_string(XRAY_CONFIG_PATH)
        .map_err(|e| format!("Ошибка чтения конфигурации: {}", e))?;
    
    let mut config: Value = serde_json::from_str(&config_data)
        .map_err(|e| format!("Ошибка парсинга JSON: {}", e))?;

    if let Some(inbounds) = config["inbounds"].as_array_mut() {
        for inbound in inbounds {
            if inbound["tag"] == "vless-inbound" {
                if let Some(clients) = inbound["settings"]["clients"].as_array_mut() {
                    let new_client = json!({
                        "id": uuid,
                        "flow": "xtls-rprx-vision",
                        "connLimit": conn_limit
                    });
                    
                    clients.push(new_client);
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
        .map_err(|e| format!("Ошибка при перезапуске Xray: {}", e))?;

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
        .map_err(|e| format!("Ошибка при перезапуске Xray: {}", e))?;

    Ok(())
}

fn check_user_in_xray_config(uuid: &str) -> bool {
    let config_data = match fs::read_to_string(XRAY_CONFIG_PATH) {
        Ok(data) => data,
        Err(_) => return false,
    };

    let config: Value = match serde_json::from_str(&config_data) {
        Ok(config) => config,
        Err(_) => return false,
    };

    if let Some(inbounds) = config["inbounds"].as_array() {
        for inbound in inbounds {
            if inbound["tag"] == "vless-inbound" {
                if let Some(clients) = inbound["settings"]["clients"].as_array() {
                    return clients.iter().any(|client| client["id"] == uuid);
                }
            }
        }
    }

    false
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

async fn add(
    path: web::Path<String>,
    data: web::Json<i32>,
) -> HttpResponse {
    let uuid = path.into_inner();
    let conn_limit = data.into_inner();

    if check_user_in_xray_config(&uuid) {
        return HttpResponse::Conflict().body("Пользователь уже существует в конфиге");
    }

    match update_xray_config(&uuid, conn_limit) {
        Ok(_) => HttpResponse::Ok().json(json!({
            "status": "success",
            "message": "Пользователь добавлен",
            "uuid": uuid,
            "conn_limit": conn_limit
        })),
        Err(e) => HttpResponse::InternalServerError().body(format!("Ошибка: {}", e)),
    }
}

async fn remove(
    uuid: web::Path<String>,
) -> HttpResponse {
    let uuid = uuid.into_inner();

    if !check_user_in_xray_config(&uuid) {
        return HttpResponse::NotFound().body("Пользователь не найден в конфиге");
    }

    match remove_user_from_xray_config(&uuid) {
        Ok(_) => HttpResponse::Ok().json(json!({
            "status": "success",
            "message": "Пользователь удален",
            "uuid": uuid
        })),
        Err(e) => HttpResponse::InternalServerError().body(format!("Ошибка: {}", e)),
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    dotenv::dotenv().ok();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect(&std::env::var("DATABASE_URL").unwrap())
        .await
        .unwrap();

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
            .service(web::resource("/add/{uuid}").route(web::post().to(add)))
            .service(web::resource("/remove/{uuid}").route(web::post().to(remove)))
    })
    .bind_openssl("0.0.0.0:443", builder)?
    .run()
    .await
}
