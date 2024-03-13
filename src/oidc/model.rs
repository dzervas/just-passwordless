use chrono::{NaiveDateTime, Utc};
use log::warn;
use serde::{Deserialize, Serialize};
use sqlx::prelude::FromRow;
use sqlx::{query, query_as, SqlitePool};

use crate::error::{Error, AppErrorKind};
use crate::CONFIG;
use crate::oidc::AuthorizeRequest;
use crate::user::{random_string, User};
use crate::error::SqlResult;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OIDCClient {
	pub id: String,
	pub secret: String,
	pub redirect_uris: Vec<String>,
	pub realms: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, FromRow)]
pub struct OIDCSession {
	pub code: String,
	pub email: String,
	pub expires_at: NaiveDateTime,
	#[sqlx(flatten)]
	pub request: AuthorizeRequest,
}

impl OIDCSession {
	pub async fn generate(db: &SqlitePool, email: String, request: AuthorizeRequest) -> std::result::Result<OIDCSession, Error> {
		let config_client = CONFIG.oidc_clients
			.iter()
			.find(|c| c.id == request.client_id);

		if config_client.is_none() {
			return Err(AppErrorKind::InvalidClientID.into());
		}

		let expires_at = Utc::now().naive_utc().checked_add_signed(CONFIG.oidc_code_duration).unwrap();
		let code = random_string();
		query!(
				"INSERT INTO oidc_codes (code, email, expires_at, scope, response_type, client_id, redirect_uri, state) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
				code,
				email,
				expires_at,
				request.scope,
				request.response_type,
				request.client_id,
				request.redirect_uri,
				request.state,
			)
			.execute(db)
			.await?;

		Ok(OIDCSession {
			code,
			email,
			expires_at,
			request,
		})
	}

	pub async fn from_code(db: &SqlitePool, code: &str) -> SqlResult<Option<(OIDCClient, OIDCSession)>> {
		println!("Looking for code: {}", code);

		// We need the non-macro query_as to support struct flattening
		let session: Option<OIDCSession> = sqlx::query_as("SELECT * FROM oidc_codes WHERE code = ?")
			.bind(code)
			.fetch_optional(db)
			.await?;

		if let Some(record) = &session {
			query!("DELETE FROM oidc_codes WHERE code = ?", record.code)
				.execute(db)
				.await?;

			if record.expires_at <= Utc::now().naive_utc() {
				return Ok(None);
			}

			let redirect_url = urlencoding::decode(&record.request.redirect_uri).unwrap().to_string();

			let config_client = CONFIG.oidc_clients
				.iter()
				.find(|c|
					c.id == record.request.client_id &&
					c.redirect_uris.contains(&redirect_url));

			if let Some(client) = config_client {
				return Ok(Some((client.clone(), record.clone())));
			}
		}

		Ok(None)
	}

	pub fn get_redirect_url(&self) -> Option<String> {
		let redirect_url = urlencoding::decode(&self.request.redirect_uri).unwrap().to_string();

		let config_client = CONFIG.oidc_clients
			.iter()
			.find(|c|
				c.id == self.request.client_id &&
				c.redirect_uris.contains(&redirect_url));

		if config_client.is_none() {
			warn!("Invalid redirect_uri: {} for client_id: {}", redirect_url, self.request.client_id);
			return None;
		}

		Some(format!("{}?code={}&state={}",
			redirect_url,
			self.code,
			self.request.state.clone().unwrap_or_default()))
	}
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OIDCAuth {
	pub auth: String,
	pub email: String,
	pub expires_at: NaiveDateTime,
}

impl OIDCAuth {
	pub async fn generate(db: &SqlitePool, email: String) -> SqlResult<OIDCAuth> {
		let expires_at = Utc::now().naive_utc().checked_add_signed(CONFIG.session_duration.to_owned()).unwrap();
		let auth = random_string();
		query!(
				"INSERT INTO oidc_auth (auth, email, expires_at) VALUES (?, ?, ?)",
				auth,
				email,
				expires_at
			)
			.execute(db)
			.await?;

		Ok(OIDCAuth {
			auth,
			email,
			expires_at,
		})
	}

	pub async fn get_user(db: &SqlitePool, auth: &str) -> SqlResult<Option<User>> {
		let auth_res = query_as!(OIDCAuth, "SELECT * FROM oidc_auth WHERE auth = ?", auth)
			.fetch_optional(db)
			.await?;

		if let Some(record) = auth_res {
			if record.expires_at <= Utc::now().naive_utc() {
				query!("DELETE FROM oidc_auth WHERE auth = ?", auth)
					.execute(db)
					.await?;
				return Ok(None)
			}

			return Ok(User::from_config(&record.email));
		}
		Ok(None)
	}
}
