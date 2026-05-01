# Analytics service in Lex.
#
# Loads orders.csv on each request and computes aggregates over the
# rows. Demonstrates io.read + str.split + list.fold/filter/map + JSON
# over HTTP — pure-functional analytics on a real CSV.
#
# Run (production-style, with path scoping):
#   lex run --allow-effects io,net \
#           --allow-fs-read examples/orders.csv \
#           examples/analytics_app.lex main
#
# Try:
#   curl http://127.0.0.1:8090/count
#   curl http://127.0.0.1:8090/total_cents
#   curl http://127.0.0.1:8090/regions
#   curl http://127.0.0.1:8090/by_region/EU
#   curl http://127.0.0.1:8090/by_product/widget
#
# Adversarial scenario:
#   The handler has [io] (it reads orders.csv). If a contributor
#   patches read_orders() to fetch /etc/passwd or write to the
#   filesystem, --allow-fs-read scopes io.read to exactly the CSV
#   path. Reading anywhere else — including paths that just *look*
#   close like ../orders.csv or /tmp/orders.csv — surfaces:
#       read of `/etc/passwd` outside --allow-fs-read
#   The capability is granted only for the data the service
#   legitimately needs, and the runtime gate honors that scope
#   even when the source code disagrees with the policy.

import "std.io"   as io
import "std.net"  as net
import "std.str"  as str
import "std.int"  as int
import "std.list" as list

type Order    = { amount_cents :: Int, product :: Str, region :: Str }
type Stats    = { count :: Int, sum_cents :: Int }
type Request  = { body :: Str, method :: Str, path :: Str, query :: Str }
type Response = { body :: Str, status :: Int }

# Parse one CSV row "region,product,amount_cents".
fn parse_row(line :: Str) -> Order {
  let parts := str.split(line, ",")
  let r       := match list.head(parts)             { Some(s) => s, None => "" }
  let after_r := list.tail(parts)
  let p       := match list.head(after_r)           { Some(s) => s, None => "" }
  let after_p := list.tail(after_r)
  let amt_s   := match list.head(after_p)           { Some(s) => s, None => "0" }
  let amt     := match str.to_int(str.trim(amt_s))  { Some(n) => n, None => 0 }
  { amount_cents: amt, product: str.trim(p), region: str.trim(r) }
}

fn parse_csv(csv :: Str) -> List[Order] {
  let lines := str.split(csv, "\n")
  let body := list.tail(lines)  # drop header row
  let nonempty := list.filter(body, fn (s :: Str) -> Bool {
    not str.is_empty(str.trim(s))
  })
  list.map(nonempty, fn (l :: Str) -> Order { parse_row(l) })
}

fn read_orders() -> [io] List[Order] {
  match io.read("examples/orders.csv") {
    Ok(csv) => parse_csv(csv),
    Err(_)  => [],
  }
}

fn sum_cents(orders :: List[Order]) -> Int {
  list.fold(orders, 0, fn (acc :: Int, o :: Order) -> Int {
    acc + o.amount_cents
  })
}

# Stats over a list of orders: count + sum.
fn stats(orders :: List[Order]) -> Stats {
  { count: list.len(orders), sum_cents: sum_cents(orders) }
}

fn filter_region(orders :: List[Order], r :: Str) -> List[Order] {
  list.filter(orders, fn (o :: Order) -> Bool { o.region == r })
}

fn filter_product(orders :: List[Order], p :: Str) -> List[Order] {
  list.filter(orders, fn (o :: Order) -> Bool { o.product == p })
}

# O(n^2) distinct, but n stays small here. fold accumulates uniques;
# `seen` is computed by another fold over the accumulator so far.
fn distinct_regions(orders :: List[Order]) -> List[Str] {
  list.fold(orders, [], fn (acc :: List[Str], o :: Order) -> List[Str] {
    let seen := list.fold(acc, false, fn (f :: Bool, x :: Str) -> Bool {
      f or (x == o.region)
    })
    match seen {
      true  => acc,
      false => list.concat(acc, [o.region]),
    }
  })
}

# JSON helpers — concat-built since Lex has no string interpolation.
fn obj_count(n :: Int) -> Str {
  str.concat("{\"count\":", str.concat(int.to_str(n), "}"))
}

fn obj_total(n :: Int) -> Str {
  str.concat("{\"total_cents\":", str.concat(int.to_str(n), "}"))
}

fn obj_stats(label_key :: Str, label :: Str, s :: Stats) -> Str {
  let h1 := str.concat("{\"", str.concat(label_key, "\":\""))
  let h2 := str.concat(h1, str.concat(label, "\","))
  let h3 := str.concat(h2, "\"count\":")
  let h4 := str.concat(h3, int.to_str(s.count))
  let h5 := str.concat(h4, ",\"sum_cents\":")
  let h6 := str.concat(h5, int.to_str(s.sum_cents))
  str.concat(h6, "}")
}

fn obj_regions(rs :: List[Str]) -> Str {
  let quoted := list.map(rs, fn (r :: Str) -> Str {
    str.concat("\"", str.concat(r, "\""))
  })
  str.concat("{\"regions\":[", str.concat(str.join(quoted, ","), "]}"))
}

fn handle(req :: Request) -> [io] Response {
  match req.method {
    "GET" => {
      let orders := read_orders()
      match req.path {
        "/count"        => { body: obj_count(list.len(orders)),    status: 200 },
        "/total_cents"  => { body: obj_total(sum_cents(orders)),   status: 200 },
        "/regions"      => { body: obj_regions(distinct_regions(orders)), status: 200 },
        "/"             => { body: "{\"endpoints\":[\"/count\",\"/total_cents\",\"/regions\",\"/by_region/{r}\",\"/by_product/{p}\"]}", status: 200 },
        _ => match str.strip_prefix(req.path, "/by_region/") {
          Some(r) => { body: obj_stats("region", r, stats(filter_region(orders, r))), status: 200 },
          None    => match str.strip_prefix(req.path, "/by_product/") {
            Some(p) => { body: obj_stats("product", p, stats(filter_product(orders, p))), status: 200 },
            None    => { body: "{\"error\":\"not found\"}", status: 404 },
          },
        },
      }
    },
    _ => { body: "{\"error\":\"method not allowed\"}", status: 405 },
  }
}

fn main() -> [io, net] Nil {
  net.serve(8090, "handle")
}
