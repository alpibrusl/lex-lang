fn safe(p :: Str) -> [fs_read("/tmp")] Str {
  match io.read(p) {
    Ok(s) => s,
    Err(e) => "denied"
  }
}
