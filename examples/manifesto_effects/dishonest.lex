# lex-lang — manifesto demo: the type system refuses a mislabeled effect
#
# This is the DISHONEST twin of honest.lex. The body makes the exact
# same network call, but the signature LIES: it claims `[io]` (local
# I/O only), hiding the fact that it reaches the network.
#
# The checker REJECTS it. You cannot mislabel what a function may do —
# the `[net]` effect reached in the body is not in the declared row, so
# `lex check` fails with an effect-not-declared error. This is the
# enforcement behind "the type system tells the caller": a `[net]`-free
# signature is a *guarantee*, not a comment.
#
#   lex check examples/manifesto_effects/dishonest.lex   # → FAILS (by design)
#
# run.sh asserts this failure. A passing `lex check` here would mean the
# manifesto's central claim had regressed.

import "std.http" as http

# Claims [io], but the body touches the network. Rejected.
fn fetch(url :: Str) -> [io] Bool {
  match http.get(url) {
    Ok(_) => true,
    Err(_) => false,
  }
}

