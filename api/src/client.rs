// Copyright 2019 The Grin Developers
// Copyright 2024 The MWC Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! High level JSON/HTTP client API

use crate::core::global;
use crate::rest::Error;
use crate::util::to_base64;
use http::uri::Uri;
use hyper::body;
use hyper::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use hyper::{Body, Client, Request};
use hyper_timeout::TimeoutConnector;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::runtime::Builder;

// Client Request Timeout
pub struct TimeOut {
	pub connect: Duration,
	pub read: Duration,
	pub write: Duration,
}

impl TimeOut {
	pub fn new(connect: u64, read: u64, write: u64) -> Self {
		Self {
			connect: Duration::from_secs(connect),
			read: Duration::from_secs(read),
			write: Duration::from_secs(write),
		}
	}
}

impl Default for TimeOut {
	fn default() -> TimeOut {
		TimeOut {
			connect: Duration::from_secs(20),
			read: Duration::from_secs(20),
			write: Duration::from_secs(20),
		}
	}
}

/// Helper function to easily issue a HTTP GET request against a given URL that
/// returns a JSON object. Handles request building, JSON deserialization and
/// response code checking.
/// This function spawns a new Tokio runtime, which means it is pretty inefficient for multiple
/// requests. In those situations you are probably better off creating a runtime once and spawning
/// `get_async` tasks on it
pub fn get<T>(url: &str, api_secret: Option<String>) -> Result<T, Error>
where
	for<'de> T: Deserialize<'de>,
{
	handle_request(
		build_request(url, "GET", api_secret, None)?,
		TimeOut::default(),
	)
}

/// Helper function to easily issue an async HTTP GET request against a given
/// URL that returns a future. Handles request building, JSON deserialization
/// and response code checking.
pub async fn get_async<T>(url: &str, api_secret: Option<String>) -> Result<T, Error>
where
	for<'de> T: Deserialize<'de> + Send + 'static,
{
	handle_request_async(build_request(url, "GET", api_secret, None)?).await
}

/// Helper function to easily issue a HTTP GET request
/// on a given URL that returns nothing. Handles request
/// building and response code checking.
pub fn get_no_ret(url: &str, api_secret: Option<String>) -> Result<(), Error> {
	send_request(
		build_request(url, "GET", api_secret, None)?,
		TimeOut::default(),
	)?;
	Ok(())
}

/// Helper function to easily issue a HTTP POST request with the provided JSON
/// object as body on a given URL that returns a JSON object. Handles request
/// building, JSON serialization and deserialization, and response code
/// checking.
pub fn post<IN, OUT>(
	url: &str,
	api_secret: Option<String>,
	input: &IN,
	timeout: TimeOut,
) -> Result<OUT, Error>
where
	IN: Serialize,
	for<'de> OUT: Deserialize<'de>,
{
	let req = create_post_request(url, api_secret, input)?;
	handle_request(req, timeout)
}

/// Helper function to easily issue an async HTTP POST request with the
/// provided JSON object as body on a given URL that returns a future. Handles
/// request building, JSON serialization and deserialization, and response code
/// checking.
pub async fn post_async<IN, OUT>(
	url: &str,
	input: &IN,
	api_secret: Option<String>,
) -> Result<OUT, Error>
where
	IN: Serialize,
	OUT: Send + 'static,
	for<'de> OUT: Deserialize<'de>,
{
	handle_request_async(create_post_request(url, api_secret, input)?).await
}

/// Helper function to easily issue a HTTP POST request with the provided JSON
/// object as body on a given URL that returns nothing. Handles request
/// building, JSON serialization, and response code
/// checking.
pub fn post_no_ret<IN>(url: &str, api_secret: Option<String>, input: &IN) -> Result<(), Error>
where
	IN: Serialize,
{
	send_request(
		create_post_request(url, api_secret, input)?,
		TimeOut::default(),
	)?;
	Ok(())
}

/// Helper function to easily issue an async HTTP POST request with the
/// provided JSON object as body on a given URL that returns a future. Handles
/// request building, JSON serialization and deserialization, and response code
/// checking.
pub async fn post_no_ret_async<IN>(
	url: &str,
	api_secret: Option<String>,
	input: &IN,
) -> Result<(), Error>
where
	IN: Serialize,
{
	send_request_async(
		create_post_request(url, api_secret, input)?,
		TimeOut::default(),
	)
	.await?;
	Ok(())
}

fn build_request(
	url: &str,
	method: &str,
	api_secret: Option<String>,
	body: Option<String>,
) -> Result<Request<Body>, Error> {
	let basic_auth_key = if global::is_mainnet() {
		"mwcmain"
	} else if global::is_floonet() {
		"mwcfloo"
	} else {
		"mwc"
	};

	build_request_ex(
		url,
		method,
		api_secret,
		Some(basic_auth_key.to_string()),
		body,
	)
}

fn build_request_ex(
	url: &str,
	method: &str,
	api_secret: Option<String>,
	basic_auth_key: Option<String>,
	body: Option<String>,
) -> Result<Request<Body>, Error> {
	// Checking only. Uri has issues with Builder 'uri()' method
	let _ = url
		.parse::<Uri>()
		.map_err::<Error, _>(|e| Error::Argument(format!("Invalid url {}, {}", url, e)))?;

	let mut builder = Request::builder();
	if basic_auth_key.is_some() && api_secret.is_some() {
		let basic_auth = format!(
			"Basic {}",
			to_base64(&format!(
				"{}:{}",
				basic_auth_key.unwrap(),
				api_secret.unwrap()
			))
		);
		builder = builder.header(AUTHORIZATION, basic_auth);
	}

	builder
		.method(method)
		.uri(url)
		.header(USER_AGENT, "mwc-client")
		.header(ACCEPT, "application/json")
		.header(CONTENT_TYPE, "application/json")
		.body(match body {
			None => Body::empty(),
			Some(json) => json.into(),
		})
		.map_err(|e| Error::RequestError(format!("Bad request {} {}: {}", method, url, e)))
}

pub fn create_post_request<IN>(
	url: &str,
	api_secret: Option<String>,
	input: &IN,
) -> Result<Request<Body>, Error>
where
	IN: Serialize,
{
	let json = serde_json::to_string(input).map_err(|e| {
		Error::Internal(format!("Post Request, Can't serialize data to JSON, {}", e))
	})?;
	build_request(url, "POST", api_secret, Some(json))
}

pub fn create_post_request_ex<IN>(
	url: &str,
	api_secret: Option<String>,
	basic_auth_key: Option<String>,
	input: &IN,
) -> Result<Request<Body>, Error>
where
	IN: Serialize,
{
	let json = serde_json::to_string(input).map_err(|e| {
		Error::Internal(format!("Post Request, Can't serialize data to JSON, {}", e))
	})?;
	build_request_ex(url, "POST", api_secret, basic_auth_key, Some(json))
}

fn handle_request<T>(req: Request<Body>, timeout: TimeOut) -> Result<T, Error>
where
	for<'de> T: Deserialize<'de>,
{
	let data = send_request(req, timeout)?;
	serde_json::from_str(&data)
		.map_err(|e| Error::ResponseError(format!("Cannot parse response: {}, {}", data, e)))
}

async fn handle_request_async<T>(req: Request<Body>) -> Result<T, Error>
where
	for<'de> T: Deserialize<'de> + Send + 'static,
{
	let data = send_request_async(req, TimeOut::default()).await?;
	let ser = serde_json::from_str(&data)
		.map_err(|e| Error::ResponseError(format!("Cannot parse response: {}, {}", data, e)))?;
	Ok(ser)
}

async fn send_request_async(req: Request<Body>, timeout: TimeOut) -> Result<String, Error> {
	let https = hyper_rustls::HttpsConnectorBuilder::new()
		.with_native_roots()
		.https_or_http()
		.enable_http1()
		.build();

	let (connect, read, write) = (
		Some(timeout.connect),
		Some(timeout.read),
		Some(timeout.write),
	);
	let mut connector = TimeoutConnector::new(https);
	connector.set_connect_timeout(connect);
	connector.set_read_timeout(read);
	connector.set_write_timeout(write);
	let client = Client::builder().build::<_, Body>(connector);

	let resp = client
		.request(req)
		.await
		.map_err(|e| Error::RequestError(format!("Cannot make request: {}", e)))?;

	let status = resp.status().clone();

	// Read body first because we want to return it in case of error.
	let raw = body::to_bytes(resp.into_body())
		.await
		.map_err(|e| Error::RequestError(format!("Cannot read response body: {}", e)))?;
	let response_body = String::from_utf8_lossy(&raw).to_string();

	if !status.is_success() {
		return Err(Error::RequestError(format!(
			"Wrong response code: {} with data {}",
			status, response_body
		)));
	}
	Ok(response_body)
}

pub fn send_request(req: Request<Body>, timeout: TimeOut) -> Result<String, Error> {
	let rt = Builder::new_current_thread()
		.enable_all()
		.build()
		.map_err(|e| Error::RequestError(format!("can't create Tokio runtime, {}", e)))?;
	rt.block_on(send_request_async(req, timeout))
}
