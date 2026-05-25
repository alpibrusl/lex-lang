# lex-lang — manifesto demo: effects are machine-verifiable constraints
#
# Manifesto §IV:
#   "An agent that generates a function with a [net] effect row does not
#    need to read the function body to know it touches the network. The
#    type system tells it. The type system tells the caller. Trust
#    without comprehension."
#
# This file is the HONEST half: every effect row tells the truth, so it
# type-checks. A caller reads the signatures alone and knows exactly
# what each function may touch — without reading a single body.
#
#   lex check examples/manifesto_effects/honest.lex      # → passes
#
# Its dishonest twin (dishonest.lex) makes the same network call under a
# lying signature and is REJECTED by the checker. See run.sh.

import "std.http" as http

# The signature says [net]. That declaration alone is the contract: any
# caller — human or agent — knows this function may reach the network,
# without inspecting the body.
fn fetch(url :: Str) -> [net] Bool {
  match http.get(url) {
    Ok(_) => true,
    Err(_) => false,
  }
}

# The signature declares no effects. That alone certifies purity: this
# function cannot touch the network, the disk, or the clock — the
# checker forbids it. The `examples {}` block runs at `lex check` time.
fn double(n :: Int) -> Int
  examples {
    double(21) => 42,
    double(0) => 0
  }
{
  n * 2
}

