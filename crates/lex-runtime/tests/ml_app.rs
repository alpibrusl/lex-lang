//! Integration tests for the ML example: train linear & logistic
//! regression on a tiny housing dataset, predict over HTTP.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap()
        .parent().unwrap()
        .to_path_buf()
}

fn spawn_ml_server(port: u16) {
    let csv_abs = workspace_root().join("examples/houses.csv");
    let csv_path = csv_abs.to_str().expect("utf-8 path");
    let src = include_str!("../../../examples/ml_app.lex")
        .replace("net.serve(8100,", &format!("net.serve({port},"))
        .replace("examples/houses.csv", csv_path);
    let prog = parse_source(&src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors: {errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let mut policy = Policy::pure();
    policy.allow_effects = ["io".to_string(), "net".to_string()]
        .into_iter().collect::<BTreeSet<_>>();
    thread::spawn(move || {
        let handler = DefaultHandler::new(policy).with_program(Arc::clone(&bc));
        let mut vm = Vm::with_handler(&bc, Box::new(handler));
        let _ = vm.call("main", vec![]);
    });
    thread::sleep(Duration::from_millis(200));
}

fn http(port: u16, path: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").unwrap_or((&buf, ""));
    let status = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
    (status, body.to_string())
}

/// Strip "{\"key\":VALUE}" → VALUE.parse::<f64>().
fn extract_value(body: &str, key: &str) -> f64 {
    let needle = format!("\"{key}\":");
    let i = body.find(&needle).unwrap_or_else(|| panic!("key {key} not in {body}")) + needle.len();
    let tail = &body[i..];
    let end = tail.find(['}', ',']).unwrap_or(tail.len());
    tail[..end].trim().parse().unwrap_or_else(|_| panic!("not a float: {body}"))
}

#[test]
fn linreg_predicts_in_range() {
    let port = 18301;
    spawn_ml_server(port);

    // 2000 sqft, 3 bedrooms → looking at the dataset, this row exists
    // (price 365k). After 400 iterations of GD, the predicted price
    // should be within ~30k of any nearby row.
    let (status, body) = http(port, "/predict_price?sqft=2000&bedrooms=3");
    assert_eq!(status, 200, "body: {body}");
    let pred = extract_value(&body, "price_thousands");
    assert!(pred > 300.0 && pred < 400.0,
        "predict_price(2000, 3) = {pred}, expected near 350k");

    // Larger house should predict higher.
    let (status, body) = http(port, "/predict_price?sqft=2500&bedrooms=4");
    assert_eq!(status, 200);
    let pred_big = extract_value(&body, "price_thousands");
    assert!(pred_big > 380.0 && pred_big < 480.0,
        "predict_price(2500, 4) = {pred_big}, expected near 440k");

    assert!(pred_big > pred,
        "bigger house should predict higher: {pred_big} vs {pred}");
}

#[test]
fn logreg_classifies_luxury() {
    let port = 18302;
    spawn_ml_server(port);

    // A small house should get LOW luxury probability.
    let (status, body) = http(port, "/predict_luxury?sqft=1200&bedrooms=2");
    assert_eq!(status, 200, "body: {body}");
    let p_low = extract_value(&body, "p_luxury");
    assert!(p_low < 0.3, "small house p_luxury = {p_low}, expected < 0.3");

    // A big house should get HIGH luxury probability.
    let (status, body) = http(port, "/predict_luxury?sqft=2500&bedrooms=4");
    assert_eq!(status, 200);
    let p_high = extract_value(&body, "p_luxury");
    assert!(p_high > 0.6, "big house p_luxury = {p_high}, expected > 0.6");
}

#[test]
fn ml_unknown_endpoint_404() {
    let port = 18303;
    spawn_ml_server(port);
    let (status, _body) = http(port, "/unknown");
    assert_eq!(status, 404);
}
