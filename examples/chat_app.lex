# Multi-user WebSocket chat in Lex.
#
# Each connection joins a room derived from its URL path:
#   ws://127.0.0.1:9090/lobby      → room "lobby"
#   ws://127.0.0.1:9090/general    → room "general"
#
# Run:
#   lex run --allow-effects net,chat examples/chat_app.lex main
# Open examples/chat_client.html in two tabs to see the broadcast.

import "std.net" as net
import "std.chat" as chat
import "std.str" as str
import "std.int" as int

type WsEvent = { body :: Str, conn_id :: Int, room :: Str }

# Each incoming text frame becomes a Lex call. We prefix the message
# with the sender's connection id so other users can see who said what,
# then broadcast to everyone in the same room (sender included — a
# real client filters its own echoes).
fn on_message(ev :: WsEvent) -> [chat] Nil {
  let prefix := str.concat("[", str.concat(int.to_str(ev.conn_id), "] "))
  let line   := str.concat(prefix, ev.body)
  chat.broadcast(ev.room, line)
}

fn main() -> [chat, net] Nil {
  net.serve_ws(9090, "on_message")
}
