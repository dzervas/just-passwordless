use actix_session::{Session, SessionMiddleware};
use actix_session::storage::CookieSessionStore;
use actix_web::{get, post, web, App, HttpResponse, HttpServer, Responder};
use actix_web::cookie::{Key, SameSite};
use chrono::Duration;
use config::ConfigFile;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqlitePool;
use lazy_static::lazy_static;
use toml;

use std::env;

pub mod config;
pub mod user;

use user::{UserLink, UserSession};

use crate::config::ConfigFileRaw;

pub(crate) const RANDOM_STRING_LEN: usize = 32;

#[cfg(not(test))]
lazy_static! {
	static ref CONFIG_FILE: String = env::var("CONFIG_FILE").unwrap_or("config.toml".to_string());
}

#[cfg(test)]
lazy_static! {
	static ref CONFIG_FILE: String = "config.sample.toml".to_string();
}

lazy_static! {
	static ref LISTEN_HOST: String = env::var("LISTEN_HOST").unwrap_or("127.0.0.1".to_string());
	static ref LISTEN_PORT: String = env::var("LISTEN_PORT").unwrap_or("8080".to_string());
	static ref DATABASE_URL: String = env::var("DATABASE_URL").unwrap_or("database.sqlite3".to_string());
	static ref SESSION_DURATION: Duration = duration_str::parse_chrono(env::var("SESSION_DURATION").unwrap_or("1mon".to_string())).unwrap();
	static ref LINK_DURATION: Duration = duration_str::parse_chrono(env::var("LINK_DURATION").unwrap_or("12h".to_string())).unwrap();
	static ref CONFIG: ConfigFile = toml::from_str::<ConfigFileRaw>(
		&std::fs::read_to_string(CONFIG_FILE.as_str())
			.expect(format!("Unable to open config file `{:?}`", CONFIG_FILE.as_str()).as_str())
		)
		.expect(format!("Unable to parse config file `{:?}`", CONFIG_FILE.as_str()).as_str())
		.into();
	// static ref SMTP_HOST: String = env::var("SESSION_TIME").unwrap_or("1d".to_string());
	// static ref SMTP_HOST: String = env::var("SESSION_TIME").unwrap_or("1d".to_string());
}

#[get("/")]
async fn index(session: Session, db: web::Data<SqlitePool>) -> impl Responder {
	let session_id = if let Some(session) = session.get::<String>("session").unwrap_or(None) {
		session
	} else {
		return HttpResponse::Unauthorized().finish()
	};


	let _session = if let Some(session) = UserSession::from_id(&db, &session_id).await {
		session
	} else {
		return HttpResponse::Unauthorized().finish()
	};

	HttpResponse::Ok().finish()
}

#[get("/signin")]
async fn signin_get() -> impl Responder {
	// Render your HTML template for sign in
	HttpResponse::Ok().body("Signin page")
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct SigninInfo {
	email: String,
}


#[post("/signin")]
async fn signin_post(form: web::Form<SigninInfo>, db: web::Data<SqlitePool>) -> impl Responder {
	let user = if let Some(user) = CONFIG.users.iter().find_map(|u| if u.email == form.email { Some(u) } else { None }) {
		user
	} else {
		return HttpResponse::Unauthorized().finish()
	};

	let session = UserLink::new(&db, user.email.clone()).await;
	println!("Link: http://{}:{}/signin/{:?}", crate::LISTEN_HOST.as_str(), crate::LISTEN_PORT.as_str(), session);

	// Send an email here with lettre
	// Assume we have a function `send_email(email: &str, session_link: &str)` that sends the email

	// let session_link = format!("/signin/{}", session_id);
	// send_email(&info.email, &session_link);

	HttpResponse::Ok().finish()
}

#[get("/signin/{magic}")]
async fn signin_magic_action(magic: web::Path<String>, session: Session, db: web::Data<SqlitePool>) -> impl Responder {
	let user = if let Some(user) = UserLink::visit(&db, magic.clone()).await {
		user
	} else {
		return HttpResponse::Unauthorized().finish()
	};

	let user_session = if let Ok(user_session) = UserSession::new(&db, &user).await {
		user_session
	} else {
		return HttpResponse::InternalServerError().finish()
	};
	session.insert("session", user_session.session_id).unwrap();

	HttpResponse::Found().append_header(("Location", "/")).finish()
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
	let db = SqlitePool::connect(&DATABASE_URL).await.expect("Failed to create pool.");
	let secret = if let Some(secret) = config::ConfigKV::get(&db, "secret").await {
		let master = hex::decode(secret).unwrap();
		Key::from(&master)
	} else {
		let key = Key::generate();
		let master = hex::encode(key.master());

		config::ConfigKV::set(&db, "secret", &master).await.unwrap_or_else(|_| panic!("Unable to set secret in the database"));

		key
	};

	HttpServer::new(move || {
		App::new()
			.app_data(web::Data::new(db.clone()))
			.service(index)
			.service(signin_get)
			.service(signin_post)
			.service(signin_magic_action)
			.wrap(
				SessionMiddleware::builder(
					CookieSessionStore::default(),
					secret.clone()
				)
				.cookie_same_site(SameSite::Strict)
				.build())
	})
	.bind(format!("{}:{}", LISTEN_HOST.as_str(), LISTEN_PORT.as_str()))?
	.run()
	.await
}

#[cfg(test)]
mod tests {
	use super::*;

	use actix_web::cookie::Cookie;
use actix_web::http::StatusCode;
	use actix_web::test;
	use chrono::Utc;
	use sqlx::query;

	pub async fn db_connect() -> SqlitePool {
		SqlitePool::connect("sqlite://database.sqlite3").await.expect("Failed to create pool.")
	}

	#[actix_web::test]
	async fn test_signin_get() {
		let mut app = test::init_service(App::new().service(signin_get)).await;

		let req = test::TestRequest::get()
			.uri("/signin")
			.to_request();

		let resp = test::call_service(&mut app, req).await;
		assert_eq!(resp.status(), StatusCode::OK);
		// assert_eq!(resp.headers().get("Content-Type").unwrap(), "text/html; charset=utf-8");
	}

	#[actix_web::test]
	async fn test_signin_post() {
		let db = &db_connect().await;
		let mut app = test::init_service(
			App::new()
				.app_data(web::Data::new(db.clone()))
				.service(signin_post)
		)
		.await;

		// Login
		let req = test::TestRequest::post()
			.uri("/signin")
			.set_form(&SigninInfo { email: "valid@example.com".to_string() })
			.to_request();

		let resp = test::call_service(&mut app, req).await;
		assert_eq!(resp.status(), StatusCode::OK);

		// Invalid login
		let req = test::TestRequest::post()
			.uri("/signin")
			.set_form(&SigninInfo { email: "invalid@example.com".to_string() })
			.to_request();

		let resp = test::call_service(&mut app, req).await;
		assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
	}

	#[actix_web::test]
	async fn test_signin_magic_action() {
		let db = &db_connect().await;
		let mut app = test::init_service(
			App::new()
				.app_data(web::Data::new(db.clone()))
				.service(signin_magic_action)
		)
		.await;

		let expiry = Utc::now().naive_utc() + chrono::Duration::try_days(1).unwrap();
		query!("INSERT INTO links (magic, email, expires_at) VALUES (?, ?, ?) ON CONFLICT(magic) DO UPDATE SET expires_at = ?",
				"valid_magic_link",
				"valid@example.com",
				expiry,
				expiry,
			)
			.execute(db)
			.await
			.unwrap();

		// Assuming a valid session exists in the database
		let req = test::TestRequest::get()
			.uri("/signin/valid_magic_link")
			.to_request();

		let resp = test::call_service(&mut app, req).await;
		assert_eq!(resp.status(), StatusCode::FOUND);

		// Assuming an invalid session
		let req = test::TestRequest::get()
			.uri("/signin/invalid_magic_link")
			.to_request();

		let resp = test::call_service(&mut app, req).await;
		assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
	}

	#[actix_web::test]
	async fn test_index() {
		let db = &db_connect().await;
		let secret = Key::generate();
		let mut app = test::init_service(
			App::new()
				.app_data(web::Data::new(db.clone()))
				.service(index)
				.wrap(
					SessionMiddleware::builder(
						CookieSessionStore::default(),
						secret
					)
					.cookie_same_site(SameSite::Strict)
					.build())
		)
		.await;

		let expiry = Utc::now().naive_utc() + chrono::Duration::try_days(1).unwrap();
		query!("INSERT INTO sessions (session_id, email, expires_at) VALUES (?, ?, ?) ON CONFLICT(session_id) DO UPDATE SET expires_at = ?",
				"valid_session_id",
				"valid@example.com",
				expiry,
				expiry,
			)
			.execute(db)
			.await
			.unwrap();

		// let req = test::TestRequest::get()
		// 	.uri("/")
		// 	.cookie(Cookie::new("session", "valid_session_id"))
		// 	.to_request();

		// let resp = test::call_service(&mut app, req).await;
		// assert_eq!(resp.status(), StatusCode::OK);

		let req = test::TestRequest::get()
			.uri("/")
			.cookie(Cookie::new("session", "invalid_session_id"))
			.to_request();

		let resp = test::call_service(&mut app, req).await;
		assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

		let req = test::TestRequest::get()
			.uri("/")
			.to_request();

		let resp = test::call_service(&mut app, req).await;
		assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
	}
}
