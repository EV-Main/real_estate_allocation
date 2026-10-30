use std::path::PathBuf;
/// Dev-only stand-in for a host page: the element on a bare document (no host
/// fonts/tokens/preflight — the estate host is the real one).
#[cfg(debug_assertions)]
const DEV_HARNESS: &str = r#"<!doctype html><html lang="en"><meta charset="utf-8"><body style="margin:0;background:#070d18;color:#e6e1d3"><script type="module" src="/mfe/data/mfe-real-estate-data.js"></script><mfe-real-estate-data></mfe-real-estate-data></body></html>"#;

fn main() {
	use std::{sync::Arc, time::Duration};

	use clap::Parser;
	use real_estate_allocation::{
		config::{AppConfig, LiveSettings, SettingsCommand, SettingsFlags},
		store::{SqliteStore, seed},
	};
	use v_utils::utils::exit_on_error;

	#[derive(Parser)]
	#[command(author, version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_HASH"), ")"), about, long_about = None)]
	struct Cli {
		#[command(flatten)]
		settings: SettingsFlags,
		#[command(subcommand)]
		command: Option<Command>,
	}

	#[derive(clap::Subcommand)]
	enum Command {
		/// Manage configuration: write defaults, diff against defaults, and generate the JSON Schema / Nix module.
		Config {
			#[command(subcommand)]
			cmd: SettingsCommand,
		},
		/// Manage the database: apply migrations, seed the test fixture.
		Db {
			#[command(subcommand)]
			cmd: DbCommand,
		},
	}

	#[derive(clap::Subcommand)]
	enum DbCommand {
		/// Apply any pending schema migrations.
		Migrate,
		/// Insert the sample portfolio (test fixture; no-op if the DB is non-empty).
		Seed,
		/// Delete the local DB, re-migrate, and reseed. Dev only.
		Reset,
	}

	// stdout JSON logs + OTLP logs/traces via ev::otel (HTTP; inert when
	// OTEL_EXPORTER_OTLP_ENDPOINT is unset). Held for the process lifetime.
	let _ = color_eyre::install();
	let _otel_guard = {
		use tracing_subscriber::prelude::*;
		let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,real_estate_allocation=debug"));
		let environment = std::env::var("APP_ENV").unwrap_or_else(|_| "production".to_string());
		let (guard, layers) = ev_lib::otel::telemetry(&ev_lib::otel::Config {
			traces_sample_rate: ev_lib::otel::Config::traces_sample_rate_for(&environment),
			environment,
		})
		.unzip();
		tracing_subscriber::registry().with(filter).with(tracing_subscriber::fmt::layer().json()).with(layers).init();
		guard
	};
	let Cli { settings, command } = Cli::parse();
	// `Config` runs before the config file is loaded (it may write a fresh one) and
	// never returns; only a `Db` command survives to be handled after config load.
	let db_command = match command {
		// `handle_settings_command` is `-> !` (it `process::exit`s), so this arm never yields.
		Some(Command::Config { cmd }) => AppConfig::handle_settings_command(cmd, settings),
		Some(Command::Db { cmd }) => Some(cmd),
		None => None,
	};

	let live_settings = Arc::new(exit_on_error(LiveSettings::new(settings, Duration::from_secs(5))));
	let config = live_settings.config().expect("config valid on startup").clone();

	let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

	if let Some(cmd) = db_command {
		rt.block_on(async {
			let open = || SqliteStore::open(config.db_path.as_ref());
			let res = match cmd {
				// `open` applies migrations; a bare open is the migrate step.
				DbCommand::Migrate => open().await.map(|_| ()),
				DbCommand::Seed => match open().await {
					Ok(store) => seed(&store).await,
					Err(e) => Err(e),
				},
				DbCommand::Reset => {
					let db = config.db_path.as_ref();
					for suffix in ["", "-wal", "-shm"] {
						// remove-if-present: a clean reset need not have prior files.
						let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
					}
					match open().await {
						Ok(store) => seed(&store).await,
						Err(e) => Err(e),
					}
				}
			};
			exit_on_error(res);
		});
		return;
	}

	// Content arrives via litestream (fresh volumes are restored by the deploy's
	// init container; dev pulls via `nix run .#pull-prod-db`), never fabricated on
	// boot — the server only ensures the schema is current (via `open`) and serves
	// what's there.
	let store = rt.block_on(async {
		let store = SqliteStore::open(config.db_path.as_ref()).await.expect("open sqlite store");
		// Self-extinguishing (see `import_legacy`): fatal on failure — serving with
		// bytes still on disk instead of in blobs would silently lose them later.
		exit_on_error(real_estate_allocation::store::import_legacy(&store, config.db_path.as_ref()).await);
		store
	});

	//HACK: dioxus-server 0.7.9 does not forward `LaunchBuilder::with_context`
	// to server functions, so a `consume_context` inside a `#[server]` fn panics
	// ("Must be called from inside a Dioxus runtime"). We instead attach the shared
	// state as an axum request extension, present on every server-fn request;
	// `crate::api` reads it via `FullstackContext`.
	use dioxus::server::{
		DioxusRouterExt as _, FullstackState,
		axum::{Extension, Router, routing::get},
		http::{Method, header},
	};
	use tower_http::cors::{Any, CorsLayer};
	let addr = config.socket_addr;
	let app_state = real_estate_allocation::api::AppState { store, config };
	let bundle = Bundle::load();
	// Any origin may call us: the `/api/embed` GET is public read-only and the
	// server-fn requests are token-authed (no ambient cookies), so `*` grants a
	// browser nothing a server-side client couldn't already fetch. No
	// `allow_credentials`, so `*` holds.
	let cors = CorsLayer::new()
		.allow_origin(Any)
		.allow_methods([Method::GET, Method::POST])
		.allow_headers([header::CONTENT_TYPE]);
	let mfe = Router::new().register_server_functions().with_state(FullstackState::headless()).merge(bundle.router());
	let router = Router::new()
		.nest("/mfe/data", mfe)
		.route("/health", get(|| async { "ok" }))
		.route("/api/embed/building/{id}", get(real_estate_allocation::api::building_json));
	#[cfg(debug_assertions)]
	let router = router.route("/", get(|| async { dioxus::server::axum::response::Html(DEV_HARNESS) }));
	let router = router.layer(Extension(app_state)).layer(cors);

	rt.block_on(async {
		let listener = tokio::net::TcpListener::bind(addr).await.expect("bind configured socket_addr");
		dioxus::server::axum::serve(listener, router).await.expect("server error");
	});
}

/// The page-MFE bundle (`nix build .#mfe-data`), read from `public/` beside the
/// binary. Nix store mtimes are all 1970, so `Last-Modified` is no validator: the
/// heavy files live under a content-hash directory and are cached immutably, and the
/// two names the host and `mfe!` hard-code carry that hash as their `ETag`.
struct Bundle {
	dir: PathBuf,
	rev: String,
}

impl Bundle {
	const CSS: &str = "data.css";
	// `stylesheet: "data/data.css"` in the `mfe!` call
	const GLUE: &str = "real_estate_allocation_data.js";

	fn load() -> Self {
		use std::hash::{Hash as _, Hasher as _};
		let dir = std::env::current_exe().expect("own exe path").parent().expect("exe has a parent dir").join("public");
		assert!(
			dir.join(Self::GLUE).is_file(),
			"page-MFE bundle missing at {} (build it first: see flake `mfe-data`)",
			dir.display()
		);
		let mut files = Vec::new();
		let mut stack = vec![dir.clone()];
		while let Some(d) = stack.pop() {
			for entry in std::fs::read_dir(&d).unwrap_or_else(|e| panic!("read {}: {e}", d.display())) {
				let path = entry.expect("dir entry").path();
				if path.is_dir() { stack.push(path) } else { files.push(path) }
			}
		}
		files.sort();
		let mut h = std::hash::DefaultHasher::new();
		for f in &files {
			f.strip_prefix(&dir).expect("walked from dir").hash(&mut h);
			std::fs::read(f).unwrap_or_else(|e| panic!("read {}: {e}", f.display())).hash(&mut h);
		}
		Self {
			dir,
			rev: format!("{:016x}", h.finish()),
		}
	}

	fn router<S: Clone + Send + Sync + 'static>(&self) -> dioxus::server::axum::Router<S> {
		use dioxus::server::{
			axum::{Router, http::HeaderValue, response::IntoResponse, routing::get},
			http::{HeaderMap, StatusCode, header},
		};
		use tower_http::{services::ServeDir, set_header::SetResponseHeader};

		let etag = HeaderValue::from_str(&format!("\"{}\"", self.rev)).expect("hex is a valid header");
		let revalidated = move |content_type: &'static str, body: Vec<u8>| {
			let etag = etag.clone();
			move |req: HeaderMap| {
				let etag = etag.clone();
				let body = body.clone();
				async move {
					let headers = [(header::CACHE_CONTROL, HeaderValue::from_static("no-cache")), (header::ETAG, etag.clone())];
					if req.get(header::IF_NONE_MATCH) == Some(&etag) {
						return (StatusCode::NOT_MODIFIED, headers).into_response();
					}
					(headers, [(header::CONTENT_TYPE, content_type)], body).into_response()
				}
			}
		};
		let entry = format!("import init from \"./{}/{}\";\nawait init();\n", self.rev, Self::GLUE).into_bytes();
		let css = std::fs::read(self.dir.join(Self::CSS)).expect("bundle css (checked by load's walk)");
		Router::new()
			.route("/mfe-real-estate-data.js", get(revalidated("text/javascript", entry)))
			.route(&format!("/{}", Self::CSS), get(revalidated("text/css", css)))
			.nest_service(
				&format!("/{}", self.rev),
				SetResponseHeader::overriding(ServeDir::new(&self.dir), header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000, immutable")),
			)
	}
}
