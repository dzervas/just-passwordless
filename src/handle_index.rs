use actix_session::Session;
use actix_web::{get, web, HttpResponse};
use sqlx::SqlitePool;

use crate::error::Response;
use crate::user::User;
use crate::CONFIG;

#[get("/")]
async fn index(session: Session, db: web::Data<SqlitePool>) -> Response {
	let user = if let Some(user) = User::from_session(&db, session).await? {
		user
	} else {
		return Ok(HttpResponse::Found()
			.append_header(("Location", "/login"))
			.finish())
	};

	Ok(HttpResponse::Ok()
		// TODO: Add realm
		.append_header((CONFIG.auth_url_email_header.as_str(), user.email.clone()))
		.append_header((CONFIG.auth_url_user_header.as_str(), user.username.unwrap_or_default()))
		.append_header((CONFIG.auth_url_name_header.as_str(), user.name.unwrap_or_default()))
		// .append_header((CONFIG.auth_url_realm_header.as_str(), user.realms.join(", ")))
		// TODO: Display something useful
		.body(user.email))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::tests::*;
	use crate::{SESSION_COOKIE, handle_login_link};

	use std::collections::HashMap;

	use actix_session::storage::CookieSessionStore;
	use actix_session::SessionMiddleware;
	use actix_web::cookie::{Cookie, Key, SameSite};
	use actix_web::http::StatusCode;
	use actix_web::{test as actix_test, App};
	use chrono::Utc;
	use sqlx::query;

	#[actix_web::test]
	async fn test_index() {
		let db = &db_connect().await;
		let mut session_map = HashMap::new();
		let secret = Key::from(&[0; 64]);
		session_map.insert(SESSION_COOKIE, "valid_session_id");

		let mut app = actix_test::init_service(
			App::new()
				.app_data(web::Data::new(db.clone()))
				.service(index)
				.service(handle_login_link::login_link)
				.wrap(
					SessionMiddleware::builder(
						CookieSessionStore::default(),
						secret
					)
					.cookie_secure(false)
					.cookie_same_site(SameSite::Strict)
					.build())
		)
		.await;

		let req = actix_test::TestRequest::get().uri("/").to_request();
		let resp = actix_test::call_service(&mut app, req).await;
		assert_eq!(resp.status(), StatusCode::FOUND);
		assert_eq!(resp.headers().get("Location").unwrap(), "/login");

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

		let req = actix_test::TestRequest::get().uri("/login/valid_magic_link").to_request();
		let resp = actix_test::call_service(&mut app, req).await;
		assert_eq!(resp.status(), StatusCode::FOUND);
		assert_eq!(resp.headers().get("Location").unwrap(), "/");

		let headers = resp.headers().clone();
		let cookie_header = headers.get("set-cookie").unwrap().to_str().unwrap();
		let parsed_cookie = Cookie::parse_encoded(cookie_header).unwrap();

		// TODO: Use actix-session, not plain cookie
		let req = actix_test::TestRequest::get()
			.uri("/")
			.cookie(parsed_cookie)
			.to_request();

		let resp = actix_test::call_service(&mut app, req).await;
		assert_eq!(resp.status(), StatusCode::OK);
		assert_eq!(resp.headers().get(CONFIG.auth_url_user_header.as_str()).unwrap(), "valid");
		assert_eq!(resp.headers().get(CONFIG.auth_url_email_header.as_str()).unwrap(), "valid@example.com");

		let req = actix_test::TestRequest::get()
			.uri("/")
			.to_request();

		let resp = actix_test::call_service(&mut app, req).await;
		assert_eq!(resp.status(), StatusCode::FOUND);
		assert_eq!(resp.headers().get("Location").unwrap(), "/login");
	}
}