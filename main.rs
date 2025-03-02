// main.rs

use actix_web::{get, middleware::Logger, web, App, HttpResponse, HttpServer, Responder};
use arrow_flight_sql_client::FlightSqlClient;
use clap::{Arg, Command};
use include_dir::{include_dir, Dir};
use influx_client::Client as InfluxClient;
use serde::Deserialize;
use serde_json::json;
use std::path::Path;
use telegraf::Client as TelegrafClient;
use actix_cors::Cors;

// Embed static files directly into the binary
static STATIC_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/static");

#[derive(Clone)]
struct AppState {
    influx_client: InfluxClient,
    telegraf_client: TelegrafClient,
    flight_sql_client: FlightSqlClient,
    influx_bucket: String,
}

#[derive(Deserialize)]
struct QueryRequest {
    query: String,
}

#[get("/")]
async fn index() -> HttpResponse {
    let html = STATIC_DIR
        .get_file("index.html")
        .expect("index.html not found")
        .contents_utf8()
        .expect("Invalid UTF-8 in index.html");

    HttpResponse::Ok()
        .content_type("text/html")
        .body(html)
}

#[get("/{filename:.*}")]
async fn static_files(path: web::Path<String>) -> HttpResponse {
    let filename = path.into_inner();
    let file = STATIC_DIR.get_file(&filename).unwrap_or_else(|| {
        STATIC_DIR
            .get_file("index.html")
            .expect("Fallback index.html missing")
    });

    let content_type = match Path::new(&filename).extension() {
        Some(ext) => match ext.to_str().unwrap() {
            "css" => "text/css",
            "js" => "application/javascript",
            "png" => "image/png",
            _ => "text/plain",
        },
        None => "text/html",
    };

    HttpResponse::Ok()
        .content_type(content_type)
        .body(file.contents())
}

async fn influx_query(
    state: web::Data<AppState>,
    req: web::Json<QueryRequest>,
) -> impl Responder {
    let start_time = std::time::Instant::now();
    let full_query = format!(
        r#"from(bucket:"{}") {}"#,
        state.influx_bucket,
        req.query
    );

    // Execute query and handle metrics
    let result = match state.influx_client.query(&full_query).await {
        Ok(response) => {
            let duration = start_time.elapsed().as_millis();
            let _ = state.telegraf_client.write_point(
                telegraf::Point::new("query_metrics")
                    .add_tag("type", "influx")
                    .add_tag("status", "success")
                    .add_field("duration_ms", duration as i64)
                    .add_field("query_length", req.query.len() as i64),
            );

            HttpResponse::Ok().json(response)
        }
        Err(e) => {
            let duration = start_time.elapsed().as_millis();
            let _ = state.telegraf_client.write_point(
                telegraf::Point::new("query_metrics")
                    .add_tag("type", "influx")
                    .add_tag("status", "error")
                    .add_field("duration_ms", duration as i64)
                    .add_field("error", e.to_string())
                    .add_field("query_length", req.query.len() as i64),
            );

            HttpResponse::InternalServerError().json(json!({"error": e.to_string()}))
        }
    };

    result
}

async fn flight_sql_query(
    state: web::Data<AppState>,
    req: web::Json<QueryRequest>,
) -> impl Responder {
    let start_time = std::time::Instant::now();

    match state.flight_sql_client.prepare(req.query.clone()).await {
        Ok(mut stmt) => match stmt.execute().await {
            Ok(flight_info) => {
                let mut results = Vec::new();
                for endpoint in flight_info.endpoint {
                    if let Some(ticket) = endpoint.ticket {
                        match state.flight_sql_client.do_get(ticket).await {
                            Ok(mut stream) => {
                                while let Some(batch) = stream.next().await {
                                    match batch {
                                        Ok(batch) => results.push(batch),
                                        Err(e) => {
                                            let _ = state.telegraf_client.write_point(
                                                telegraf::Point::new("query_metrics")
                                                    .add_tag("type", "flightsql")
                                                    .add_tag("status", "error")
                                                    .add_field("error", e.to_string()),
                                            );
                                            return HttpResponse::InternalServerError()
                                                .json(json!({"error": e.to_string()}));
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = state.telegraf_client.write_point(
                                    telegraf::Point::new("query_metrics")
                                        .add_tag("type", "flightsql")
                                        .add_tag("status", "error")
                                        .add_field("error", e.to_string()),
                                );
                                return HttpResponse::InternalServerError()
                                    .json(json!({"error": e.to_string()}));
                            }
                        }
                    }
                }

                let duration = start_time.elapsed().as_millis();
                let _ = state.telegraf_client.write_point(
                    telegraf::Point::new("query_metrics")
                        .add_tag("type", "flightsql")
                        .add_tag("status", "success")
                        .add_field("duration_ms", duration as i64)
                        .add_field("query_length", req.query.len() as i64),
                );

                // Convert Arrow batches to JSON - implement this based on your needs
                HttpResponse::Ok().json(json!({"data": results.len()}))
            }
            Err(e) => {
                let _ = state.telegraf_client.write_point(
                    telegraf::Point::new("query_metrics")
                        .add_tag("type", "flightsql")
                        .add_tag("status", "error")
                        .add_field("error", e.to_string()),
                );
                HttpResponse::InternalServerError().json(json!({"error": e.to_string()}))
            }
        },
        Err(e) => {
            let _ = state.telegraf_client.write_point(
                telegraf::Point::new("query_metrics")
                    .add_tag("type", "flightsql")
                    .add_tag("status", "error")
                    .add_field("error", e.to_string()),
            );
            HttpResponse::InternalServerError().json(json!({"error": e.to_string()}))
        }
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));

    let matches = Command::new("query-runner")
        .arg(Arg::new("influxdb-url")
            .long("influxdb-url")
            .env("INFLUXDB_URL")
            .default_value("http://localhost:8086"))
        .arg(Arg::new("influxdb-token")
            .long("influxdb-token")
            .env("INFLUXDB_TOKEN")
            .default_value("your-token"))
        .arg(Arg::new("influxdb-bucket")
            .long("influxdb-bucket")
            .env("INFLUXDB_BUCKET")
            .default_value("your-bucket"))
        .arg(Arg::new("flight-sql-url")
            .long("flight-sql-url")
            .env("FLIGHT_SQL_URL")
            .default_value("grpc://localhost:8082"))
        .get_matches();

    // Initialize clients
    let influx_client = InfluxClient::new(
        matches.get_one::<String>("influxdb-url").unwrap(),
        matches.get_one::<String>("influxdb-token").unwrap(),
    ).expect("Failed to create InfluxDB client");

    let telegraf_client = TelegrafClient::new().expect("Failed to create Telegraf client");

    let flight_sql_client = FlightSqlClient::new(
        matches.get_one::<String>("flight-sql-url").unwrap()
    ).await.expect("Failed to create FlightSQL client");

    let state = web::Data::new(AppState {
        influx_client,
        telegraf_client,
        flight_sql_client,
        influx_bucket: matches.get_one::<String>("influxdb-bucket").unwrap().clone(),
    });

    HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .service(index)
            .service(static_files)
            .service(web::resource("/query").route(web::post().to(influx_query)))
            .service(web::resource("/query-flightsql").route(web::post().to(flight_sql_query)))
            .wrap(Logger::default())
            .wrap(
                Cors::default()
                    .allow_any_origin()
                    .allowed_methods(vec!["GET", "POST"])
                    .allowed_headers(vec![
                        actix_web::http::header::CONTENT_TYPE,
                        actix_web::http::header::ACCEPT,
                    ])
            )
    })
    .bind("0.0.0.0:8080")?
    .run()
    .await
}
