//! One application-owned server shared by the React, Preact, and Solid examples.
//! Run a renderer's `dev` script from its directory. After its `build` script finishes,
//! run `PORT=8080 cargo run --manifest-path ../server/Cargo.toml` from that same directory.

use std::path::PathBuf;

use axum::handler::HandlerWithoutStateExt;
use axum::routing::get;
use http::StatusCode;
use serde::Serialize;
use vorma::memoized_tasks::TaskRuntime;
use vorma::{
	App, HeadEl, NoDeclaredError, Render, TerminalOutcome, TsGen, ViewLoader, ViewLoaderCtx,
};

vorma::view_pattern!(struct RootParams = "/");
vorma::view_pattern!(struct HomeParams = "/_index");
vorma::view_pattern!(struct AboutParams = "/about");

#[derive(Serialize, TsGen)]
struct RootOutput {
	title: String,
}

async fn load_root(
	_ctx: ViewLoaderCtx<(), RootParams, ()>,
) -> Result<Render<RootOutput>, TerminalOutcome<NoDeclaredError>> {
	Ok(Render::new(RootOutput {
		title: "Vorma renderer example".to_owned(),
	})
	.set_html_attr("lang", "en")
	.add_head_el(HeadEl::new("title").add_child("Vorma renderer example")))
}

async fn load_home(
	_ctx: ViewLoaderCtx<(), HomeParams, ()>,
) -> Result<Render<String>, TerminalOutcome<NoDeclaredError>> {
	Ok(Render::new(
		"The Rust loader supplied this message.".to_owned(),
	))
}

async fn load_about(
	_ctx: ViewLoaderCtx<(), AboutParams, ()>,
) -> Result<Render<()>, TerminalOutcome<NoDeclaredError>> {
	Ok(Render::new(()))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
	let app = App {
		// Each renderer configures its own dist directory relative to its working directory.
		dist_dir: PathBuf::from("dist"),
		max_request_body_size: 64 * 1024,
		task_middlewares: vec![],
		api_endpoints: vec![],
		head_dedupe_rules: vec![],
		view_loaders: vec![
			ViewLoader::new(load_root),
			ViewLoader::new(load_home),
			ViewLoader::new(load_about),
		],
	}
	.load()
	.await?;
	let service = app.service(TaskRuntime::new(()));
	let router = axum::Router::new()
		.route("/healthz", get(|| async { "ok" }))
		.fallback_service(
			service
				.set_fallback((|| async { (StatusCode::NOT_FOUND, "Not found") }).into_service()),
		)
		.layer(app.static_public_assets_layer());
	let port: u16 = std::env::var("PORT")?.parse()?;
	let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
	axum::serve(listener, router).await?;
	Ok(())
}
