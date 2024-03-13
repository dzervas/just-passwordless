use actix_session::Session;
use actix_web::{get, post, web, HttpRequest, HttpResponse, Responder};
use log::info;
use sqlx::SqlitePool;
use jwt_simple::prelude::*;

use crate::error::ErrorKind;
use crate::user::User;
use crate::{Response, CONFIG};

pub mod model;
pub mod data;

use model::{OIDCAuth, OIDCSession};
use data::*;

pub async fn init(db: &SqlitePool) -> RS256KeyPair {
	if let Some(keypair) = crate::config::ConfigKV::get(&db, "jwt_keypair").await {
		RS256KeyPair::from_pem(&keypair).expect("Failed to load JWT keypair from database")
	} else {
		log::warn!("Generating JWT keypair for RSA 4096. This is going to take some time...");
		let keypair = RS256KeyPair::generate(4096).expect("Failed to generate RSA 4096 keypair");
		let keypair_pem = keypair.to_pem().expect("Failed to convert keypair to PEM - that's super weird");

		crate::config::ConfigKV::set(&db, "jwt_keypair", &keypair_pem).await.expect("Unable to set secret in the database");

		keypair
	}
	.with_key_id("default")
}

#[get("/.well-known/openid-configuration")]
pub async fn configuration(req: HttpRequest) -> impl Responder {
	let base_url = CONFIG.url_from_request(&req);
	let discovery = Discovery::new(&base_url);
	HttpResponse::Ok().json(discovery)
}

async fn authorize(session: Session, db: web::Data<SqlitePool>, data: AuthorizeRequest) -> Response {
	info!("Beginning OIDC flow for {}", data.client_id);
	session.insert("oidc_authorize", data.clone()).unwrap();

	let user = if let Some(user) = User::from_session(&db, session).await? {
		user
	} else {
		let target_url = format!("/login?{}", serde_qs::to_string(&data)?);
		return Ok(HttpResponse::Found()
			.append_header(("Location", target_url))
			.finish())
	};

	let oidc_session = data.generate_code(&db, user.email.as_str()).await?;

	// TODO: Check the state with the cookie for CSRF
	let redirect_url = oidc_session.get_redirect_url().unwrap();
	Ok(HttpResponse::Found()
		.append_header(("Location", redirect_url.as_str()))
		.finish())
	// Either send to ?code=<code>&state=<state>
	// Or send to ?error=<error>&error_description=<error_description>&state=<state>
}

#[get("/oidc/authorize")]
pub async fn authorize_get(session: Session, db: web::Data<SqlitePool>, data: web::Query<AuthorizeRequest>) -> impl Responder {
	authorize(session, db, data.into_inner()).await
}

#[post("/oidc/authorize")]
pub async fn authorize_post(session: Session, db: web::Data<SqlitePool>, data: web::Form<AuthorizeRequest>) -> impl Responder {
	authorize(session, db, data.into_inner()).await
}

#[post("/oidc/token")]
pub async fn token(req: HttpRequest, db: web::Data<SqlitePool>, data: web::Form<TokenRequest>, key: web::Data<RS256KeyPair>) -> Response {
	let (client, session) = if let Some(client_session) = OIDCSession::from_code(&db, &data.code).await? {
		client_session
	} else {
		return Ok(HttpResponse::BadRequest().finish());
	};

	if
		&client.id != data.client_id.as_ref().unwrap_or(&String::default()) ||
		&client.secret != data.client_secret.as_ref().unwrap_or(&String::default()) {
		return Ok(HttpResponse::BadRequest().finish());
	}

	let jwt_data = JWTData {
		user: session.email.clone(),
		client_id: session.request.client_id.clone(),
		..JWTData::new(&CONFIG.url_from_request(&req))
	};
	println!("JWT Data: {:?}", jwt_data);

	// NOTE: We can crash here
	let claims = Claims::with_custom_claims(
		jwt_data,
		Duration::from_millis(
			CONFIG.session_duration
			.num_milliseconds()
			.try_into()
			.map_err(|_| ErrorKind::InvalidDuration)?));
	let id_token = key.as_ref().sign(claims)?;

	let access_token = OIDCAuth::generate(&db, session.email.clone()).await?.auth;

	Ok(HttpResponse::Ok().json(TokenResponse {
		access_token,
		token_type: "Bearer".to_string(),
		expires_in: CONFIG.session_duration.num_seconds(),
		id_token,
		refresh_token: None,
	}))
	// Either send to ?access_token=<token>&token_type=<type>&expires_in=<seconds>&refresh_token=<token>&id_token=<token>
	// Or send to ?error=<error>&error_description=<error_description>
}

#[get("/oidc/jwks")]
pub async fn jwks(key: web::Data<RS256KeyPair>) -> Response {
	let comp = key.as_ref().public_key().to_components();

	let item = JWKSResponseItem {
		modulus: Base64::encode_to_string(comp.n)?,
		exponent: Base64::encode_to_string(comp.e)?,
		..Default::default()
	};

	let resp = JwksResponse {
		keys: vec![item],
	};

	Ok(HttpResponse::Ok().json(resp))
}

#[get("/oidc/userinfo")]
pub async fn userinfo(db: web::Data<SqlitePool>, req: HttpRequest) -> Response {
	let auth_header = req.headers().get("Authorization").ok_or(ErrorKind::MissingAuthorizationHeader)?;
	let auth_header_parts = auth_header
		.to_str()
		.map_err(|_| ErrorKind::CouldNotParseAuthorizationHeader)?
		.split_whitespace()
		.collect::<Vec<&str>>();

	if auth_header_parts.len() != 2 || auth_header_parts[0] != "Bearer" {
		return Ok(HttpResponse::BadRequest().finish())
	}

	let auth = auth_header_parts[1];

	if let Ok(Some(user)) = OIDCAuth::get_user(&db, auth).await {
		let username = if let Some(alias) = user.username.clone() {
			alias
		} else {
			user.email.clone()
		};

		let resp = UserInfoResponse {
			user: user.email.clone(),
			email: user.email.clone(),
			preferred_username: username,
		};
		println!("Userinfo Response: {:?}", resp);

		Ok(HttpResponse::Ok().json(resp))
	} else {
		Ok(HttpResponse::Unauthorized().finish())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::tests::*;

	use actix_session::storage::CookieSessionStore;
	use actix_session::SessionMiddleware;
	use actix_web::cookie::Cookie;
	use actix_web::cookie::Key;
	use actix_web::App;
	use actix_web::test as actix_test;
	use actix_web::http::StatusCode;
	use chrono::Utc;
	use sqlx::query;

	#[actix_web::test]
	async fn test_oidc() {
		let db = &db_connect().await;
		let secret = Key::generate();
		let keypair = RS256KeyPair::from_pem("-----BEGIN PRIVATE KEY-----\n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\n9gv9wAIJD+X4ojJsqb8tX9sSmIiO\n-----END PRIVATE KEY-----")
			.unwrap()
			.with_key_id("default");

		let mut app = actix_test::init_service(
			App::new()
				.app_data(web::Data::new(db.clone()))
				.app_data(web::Data::new(keypair))
				.service(crate::login_magic_action)
				.service(authorize_get)
				.service(authorize_post)
				.service(token)
				.service(userinfo)
				.wrap(
					SessionMiddleware::builder(
						CookieSessionStore::default(),
						secret
					)
					.build())
		)
		.await;

		let client_id = "my_client";
		let client_secret = "my_secret";
		let redirect_url = "https://openidconnect.net/callback";
		let redirect = urlencoding::encode(redirect_url);
		let state = "my_awesome_state";

		let req = actix_test::TestRequest::get()
			.uri(format!(
				"/oidc/authorize?client_id={}&redirect_uri={}&scope=openid%20profile%20email%20phone%20address&response_type=code&state={}",
				client_id,
				redirect,
				state
			).as_str())
			.to_request();

		let resp = actix_test::call_service(&mut app, req).await;

		assert_eq!(resp.status(), StatusCode::FOUND);

		// Unauthenticated user should be redirected to login
		let target = resp.headers().get("Location").unwrap().to_str().unwrap();
		assert!(target.starts_with("/login"));

		let expiry = Utc::now().naive_utc() + chrono::Duration::try_days(1).unwrap();
		query!("INSERT INTO links (magic, email, expires_at) VALUES (?, ?, ?) ON CONFLICT(magic) DO UPDATE SET expires_at = ?",
				"oidc_valid_magic_link",
				"valid@example.com",
				expiry,
				expiry,
			)
			.execute(db)
			.await
			.unwrap();

		let req = actix_test::TestRequest::get().uri("/login/oidc_valid_magic_link").to_request();
		let resp = actix_test::call_service(&mut app, req).await;
		assert_eq!(resp.status(), StatusCode::FOUND);
		assert_eq!(resp.headers().get("Location").unwrap(), "/");

		let headers = resp.headers().clone();
		let cookie_header = headers.get("set-cookie").unwrap().to_str().unwrap();
		let parsed_cookie = Cookie::parse_encoded(cookie_header).unwrap();

		let req = actix_test::TestRequest::get()
			.uri(format!(
				"/oidc/authorize?client_id={}&redirect_uri={}&scope=openid%20profile%20email%20phone%20address&response_type=code&state={}",
				client_id,
				redirect,
				state
			).as_str())
			.cookie(parsed_cookie.clone())
			.to_request();
		let resp = actix_test::call_service(&mut app, req).await;
		assert_eq!(resp.status(), StatusCode::FOUND);
		let location_header = resp.headers().get("Location").unwrap().to_str().unwrap();
		assert!(location_header.starts_with(redirect_url));
		let location_url = reqwest::Url::parse(location_header).unwrap();
		let code = location_url.query_pairs().find(|(k, _)| k == "code").unwrap().1.to_string();
		println!("New Code: {}", code);

		let req = actix_test::TestRequest::post()
			.uri("/oidc/token")
			.set_form(&TokenRequest {
				grant_type: "authorization_code".to_string(),
				code,
				client_id: Some(client_id.to_string()),
				client_secret: Some(client_secret.to_string()),
				redirect_uri: Some(redirect.to_string()),
			})
			.to_request();
		let resp = actix_test::call_service(&mut app, req).await;
		assert_eq!(&resp.status(), &StatusCode::OK);
		let body = actix_test::read_body(resp).await;
		let resp_token = serde_json::from_slice::<TokenResponse>(&body).unwrap();

		let req = actix_test::TestRequest::get()
			.uri("/oidc/userinfo")
			.append_header(("Authorization", format!("Bearer {}", resp_token.access_token)))
			.to_request();
		let resp = actix_test::call_service(&mut app, req).await;
		assert_eq!(resp.status(), StatusCode::OK);
		let body = actix_test::read_body(resp).await;
		let resp_userinfo = serde_json::from_slice::<UserInfoResponse>(&body).unwrap();
		assert_eq!(resp_userinfo, UserInfoResponse{
			user: "valid@example.com".to_string(),
			email: "valid@example.com".to_string(),
			preferred_username: "valid".to_string(),
		})
	}
}
