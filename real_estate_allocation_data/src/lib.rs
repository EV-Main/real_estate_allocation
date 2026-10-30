#![cfg(target_arch = "wasm32")] // wasm-only bundle
//! The page-MFE shell around [`real_estate_allocation::App`]: the custom-element
//! registration (`ev_lib::mfe!`) and the server-fn root. Everything else is the
//! dashboard lib.

use dioxus::prelude::*;

ev_lib::mfe! {
	service: "real-estate", name: "data", kind: page,
	root: crate::Root, stylesheet: "data/data.css",
	messages: real_estate_allocation::i18n::catalogue
}

#[component]
fn Root() -> Element {
	// `set_server_url` panics on a second set, and the host re-creates the element
	// on every remount (StrictMode, soft navigation back) — once per wasm instance.
	static SERVER_URL: std::sync::Once = std::sync::Once::new();
	SERVER_URL.call_once(|| dioxus_fullstack::set_server_url(format!("{}/mfe/data", ev_lib::mfe::bundle_origin()).leak()));
	rsx! { real_estate_allocation::App {} }
}
