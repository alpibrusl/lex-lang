# Classical ML in Lex: linear & logistic regression on a tiny housing
# dataset, served as a /predict REST API.
#
# - Loads houses.csv, builds X and y matrices via std.math.
# - Trains a 3-feature linear regression (bias + sqft + bedrooms)
#   by N iterations of gradient descent, all expressed as a fold over
#   list.range(0, N). The loop is pure: each step returns a fresh
#   theta matrix.
# - Trains a logistic regression on the same features against a binary
#   "luxury" label (price > $380k), again via gradient descent.
# - Serves predictions over HTTP. The trained models are recomputed
#   per request — the dataset is small enough that ~200 iterations
#   complete in a few milliseconds.
#
# Run:
#   lex run --allow-effects io,net examples/ml_app.lex main
#
# Try:
#   curl 'http://127.0.0.1:8100/predict_price?sqft=2000&bedrooms=3'
#   curl 'http://127.0.0.1:8100/predict_luxury?sqft=2400&bedrooms=4'

import "std.io"   as io
import "std.net"  as net
import "std.str"  as str
import "std.int"  as int
import "std.float" as float
import "std.list" as list
import "std.math" as math

# Matrix is a built-in alias registered by std.math: it represents
# matrices of f64s. Runtime values use a flat row-major fast lane;
# Lex code shouldn't field-access them — go through math.rows /
# math.cols / math.get.

type Row      = { bedrooms :: Float, price :: Float, sqft :: Float }
type Request  = { body :: Str, method :: Str, path :: Str, query :: Str }
type Response = { body :: Str, status :: Int }

# ----- CSV parsing ---------------------------------------------------

fn parse_house(line :: Str) -> Row {
  let parts := str.split(line, ",")
  let s_str   := match list.head(parts)   { Some(s) => s, None => "0" }
  let after_s := list.tail(parts)
  let b_str   := match list.head(after_s) { Some(s) => s, None => "0" }
  let after_b := list.tail(after_s)
  let p_str   := match list.head(after_b) { Some(s) => s, None => "0" }
  let s := match str.to_float(str.trim(s_str)) { Some(f) => f, None => 0.0 }
  let b := match str.to_int(str.trim(b_str))   { Some(n) => int.to_float(n), None => 0.0 }
  let p := match str.to_float(str.trim(p_str)) { Some(f) => f, None => 0.0 }
  { bedrooms: b, price: p, sqft: s }
}

fn parse_houses(csv :: Str) -> List[Row] {
  let lines    := str.split(csv, "\n")
  let body     := list.tail(lines)
  let nonempty := list.filter(body, fn (s :: Str) -> Bool {
    not str.is_empty(str.trim(s))
  })
  list.map(nonempty, fn (l :: Str) -> Row { parse_house(l) })
}

fn read_houses() -> [io] List[Row] {
  match io.read("examples/houses.csv") {
    Ok(csv) => parse_houses(csv),
    Err(_)  => [],
  }
}

# ----- Feature engineering ------------------------------------------

# X is n×3: [1.0, sqft/1000, bedrooms]. The bias column lets the model
# learn an intercept; scaling sqft keeps gradient descent well-conditioned.
fn build_x(rows :: List[Row]) -> Matrix {
  let lists := list.map(rows, fn (r :: Row) -> List[Float] {
    [1.0, r.sqft / 1000.0, r.bedrooms]
  })
  math.from_lists(lists)
}

# y is n×1, scaled by 100 so values land near unit range.
fn build_y_price(rows :: List[Row]) -> Matrix {
  let lists := list.map(rows, fn (r :: Row) -> List[Float] {
    [r.price / 100.0]
  })
  math.from_lists(lists)
}

# Binary label: "luxury" if price > $380k. Encoded 1.0 / 0.0.
fn build_y_luxury(rows :: List[Row]) -> Matrix {
  let lists := list.map(rows, fn (r :: Row) -> List[Float] {
    match r.price > 380.0 { true => [1.0], false => [0.0] }
  })
  math.from_lists(lists)
}

# ----- Linear regression --------------------------------------------

# One gradient step:  θ ← θ − (lr · 2/n) · Xᵀ (Xθ − y)
fn linreg_step(x :: Matrix, y :: Matrix, theta :: Matrix, lr :: Float) -> Matrix {
  let pred := math.matmul(x, theta)
  let err  := math.sub(pred, y)
  let grad := math.matmul(math.transpose(x), err)
  let n    := int.to_float(math.rows(x))
  math.sub(theta, math.scale(lr * 2.0 / n, grad))
}

fn fit_linreg(x :: Matrix, y :: Matrix, iters :: Int, lr :: Float) -> Matrix {
  let theta0 := math.zeros(math.cols(x), 1)
  list.fold(list.range(0, iters), theta0, fn (theta :: Matrix, i :: Int) -> Matrix {
    linreg_step(x, y, theta, lr)
  })
}

# ----- Logistic regression ------------------------------------------

# σ(Xθ) − y, then average gradient.
fn logreg_step(x :: Matrix, y :: Matrix, theta :: Matrix, lr :: Float) -> Matrix {
  let z    := math.matmul(x, theta)
  let pred := math.sigmoid(z)
  let err  := math.sub(pred, y)
  let grad := math.matmul(math.transpose(x), err)
  let n    := int.to_float(math.rows(x))
  math.sub(theta, math.scale(lr / n, grad))
}

fn fit_logreg(x :: Matrix, y :: Matrix, iters :: Int, lr :: Float) -> Matrix {
  let theta0 := math.zeros(math.cols(x), 1)
  list.fold(list.range(0, iters), theta0, fn (theta :: Matrix, i :: Int) -> Matrix {
    logreg_step(x, y, theta, lr)
  })
}

# ----- Prediction ---------------------------------------------------

fn predict_one(theta :: Matrix, sqft :: Float, bedrooms :: Float) -> Float {
  let xv := math.from_lists([[1.0, sqft / 1000.0, bedrooms]])
  let yhat := math.matmul(xv, theta)
  math.get(yhat, 0, 0)
}

fn predict_proba_luxury(theta :: Matrix, sqft :: Float, bedrooms :: Float) -> Float {
  let xv := math.from_lists([[1.0, sqft / 1000.0, bedrooms]])
  let z := math.matmul(xv, theta)
  let p := math.sigmoid(z)
  math.get(p, 0, 0)
}

# ----- Query parsing ------------------------------------------------

# "sqft=2000&bedrooms=3" → look up sqft / bedrooms by key.
fn query_get(query :: Str, key :: Str) -> Float {
  let pairs := str.split(query, "&")
  list.fold(pairs, 0.0, fn (acc :: Float, kv :: Str) -> Float {
    let parts := str.split(kv, "=")
    let k := match list.head(parts) { Some(s) => s, None => "" }
    let v_str := match list.head(list.tail(parts)) { Some(s) => s, None => "0" }
    match k == key {
      true  => match str.to_float(v_str) { Some(f) => f, None => acc },
      false => acc,
    }
  })
}

# ----- JSON helpers -------------------------------------------------

fn json_pred(label :: Str, value :: Float) -> Str {
  let h1 := str.concat("{\"", str.concat(label, "\":"))
  str.concat(h1, str.concat(float.to_str(value), "}"))
}

# ----- Routing ------------------------------------------------------

fn handle(req :: Request) -> [io] Response {
  match req.method {
    "GET" => match req.path {
      "/" => { body: "{\"endpoints\":[\"/predict_price?sqft=&bedrooms=\",\"/predict_luxury?sqft=&bedrooms=\"]}", status: 200 },
      "/predict_price" => {
        let rows  := read_houses()
        let x     := build_x(rows)
        let y     := build_y_price(rows)
        let theta := fit_linreg(x, y, 400, 0.05)
        let sqft  := query_get(req.query, "sqft")
        let beds  := query_get(req.query, "bedrooms")
        # Model predicts price/100; multiply back to thousands.
        let price_thousands := predict_one(theta, sqft, beds) * 100.0
        { body: json_pred("price_thousands", price_thousands), status: 200 }
      },
      "/predict_luxury" => {
        let rows  := read_houses()
        let x     := build_x(rows)
        let y     := build_y_luxury(rows)
        let theta := fit_logreg(x, y, 800, 0.5)
        let sqft  := query_get(req.query, "sqft")
        let beds  := query_get(req.query, "bedrooms")
        let p := predict_proba_luxury(theta, sqft, beds)
        { body: json_pred("p_luxury", p), status: 200 }
      },
      _ => { body: "{\"error\":\"not found\"}", status: 404 },
    },
    _ => { body: "{\"error\":\"method not allowed\"}", status: 405 },
  }
}

fn main() -> [io, net] Nil {
  net.serve(8100, "handle")
}
