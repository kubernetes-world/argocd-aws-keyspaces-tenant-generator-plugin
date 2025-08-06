use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::post, Json, Router};
use once_cell::sync::OnceCell;
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, env, fs, net::SocketAddr, sync::Arc};
use thiserror::Error;
use tracing::{error, info};

// rustls bits
use rustls::{ClientConfig, RootCertStore};
use rustls::pki_types::CertificateDer;
use std::io::BufReader;
use std::fs::File;

fn main() {
    println!("Hello, world!");
}
