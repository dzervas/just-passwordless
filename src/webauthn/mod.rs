use webauthn_rs::prelude::*;

use crate::CONFIG;

pub mod handle_reg_start;
pub mod handle_reg_finish;

pub fn init() -> WebauthnResult<Webauthn> {
	// TODO: Set the origin from the config
	let rp_origin = Url::parse("http://localhost:8080").expect("Invalid webauthn URL");
	WebauthnBuilder::new("localhost", &rp_origin)?
		.rp_name(&CONFIG.title)
		.build()
}