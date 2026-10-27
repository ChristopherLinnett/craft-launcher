//! Small pieces of HTTP: URL decoding, forms, file types, and the sign-in page.

use std::path::Path;

/// Decodes `%XX` escapes (and `+` as a space in forms). `None` for bad escapes or text that
/// isn't UTF-8.
pub fn decode(text: &str, plus_is_space: bool) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'%' => {
                let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok())?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' if plus_is_space => {
                out.push(b' ');
                i += 1;
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Escapes everything but unreserved characters and `/`, for a path inside a query value.
pub fn encode(text: &str) -> String {
    text.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) { char::from(b).to_string() } else { format!("%{b:02X}") }).collect()
}

/// The value of `key` in a form body or query string.
pub fn form_value(form: &str, key: &str) -> Option<String> {
    form.split('&').filter_map(|pair| pair.split_once('=')).find(|(k, _)| *k == key).and_then(|(_, v)| decode(v, true))
}

/// A URL path's segments, decoded, or `None` if one could leave the folder it's served from.
pub fn segments(path: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for raw in path.split('/').filter(|s| !s.is_empty()) {
        let segment = decode(raw, false)?;
        if segment == "." || segment == ".." || segment.contains(['/', '\\', '\0']) {
            return None;
        }
        out.push(segment);
    }
    Some(out)
}

/// Where to go after signing in: a path on this server, else the launcher.
pub fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(n) if n.starts_with('/') && !n.starts_with("//") && !n.contains(['\\', '\r', '\n']) => n.to_string(),
        _ => "/".to_string(),
    }
}

pub fn content_type(file: &Path) -> &'static str {
    match file.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("txt" | "md") => "text/plain; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        _ => "application/octet-stream",
    }
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// The sign-in page. Plain HTML: no script runs before someone has signed in.
pub fn sign_in_page(next: &str, problem: Option<&str>) -> String {
    let problem = problem.map(|p| format!("<p class=\"problem\" role=\"alert\">{}</p>", escape_html(p))).unwrap_or_default();
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Sign in · ArtCraft Launcher</title>
<style>
  html, body {{ margin: 0; height: 100%; background: #121316; color: #f2f1ee; font: 15px/1.5 system-ui, -apple-system, "Segoe UI", sans-serif; }}
  main {{ min-height: 100%; display: flex; align-items: center; justify-content: center; padding: 24px; box-sizing: border-box; }}
  form {{ width: 100%; max-width: 360px; border: 1px solid rgba(242, 241, 238, 0.14); padding: 32px; }}
  h1 {{ margin: 0 0 4px; font-size: 22px; font-weight: 650; letter-spacing: -0.01em; }}
  p {{ margin: 0 0 24px; color: rgba(242, 241, 238, 0.62); }}
  label {{ display: block; margin-bottom: 8px; font: 500 11px/1.4 ui-monospace, SFMono-Regular, Menlo, monospace; letter-spacing: 0.12em; text-transform: uppercase; color: rgba(242, 241, 238, 0.62); }}
  input {{ width: 100%; box-sizing: border-box; padding: 10px 12px; background: #1b1c20; color: inherit; border: 1px solid rgba(242, 241, 238, 0.2); border-radius: 0; font: inherit; }}
  input:focus {{ outline: 2px solid #7fb0ff; outline-offset: -1px; }}
  button {{ margin-top: 20px; width: 100%; padding: 11px; background: #f2f1ee; color: #121316; border: 0; border-radius: 0; cursor: pointer; font: 600 12px/1.4 ui-monospace, SFMono-Regular, Menlo, monospace; letter-spacing: 0.14em; text-transform: uppercase; }}
  .problem {{ margin: 16px 0 0; color: #ff8a70; }}
  @media (prefers-color-scheme: light) {{
    html, body {{ background: #f2f1ee; color: #101014; }}
    form {{ border-color: rgba(16, 16, 20, 0.14); }}
    p, label {{ color: rgba(16, 16, 20, 0.62); }}
    input {{ background: #fff; border-color: rgba(16, 16, 20, 0.2); }}
    button {{ background: #101014; color: #f2f1ee; }}
    .problem {{ color: #b83a24; }}
  }}
</style>
</head>
<body>
<main>
<form method="post" action="/login">
  <h1>ArtCraft Launcher</h1>
  <p>Sign in with your team's password to open the Craft Apps.</p>
  <label for="password">Password</label>
  <input id="password" name="password" type="password" autocomplete="current-password" required autofocus>
  <input type="hidden" name="next" value="{next}">
  <button type="submit">Sign in</button>
  {problem}
</form>
</main>
</body>
</html>
"#,
        next = escape_html(next),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_that_could_leave_their_folder_are_refused() {
        assert_eq!(segments("/filmcraft/app/x.wasm"), Some(vec!["filmcraft".into(), "app".into(), "x.wasm".into()]));
        assert_eq!(segments("/"), Some(vec![]));
        assert_eq!(segments("//filmcraft//"), Some(vec!["filmcraft".into()]));
        for bad in ["/../config.json", "/filmcraft/%2e%2e/x", "/a/%2Fetc", "/a%5c..", "/%00", "/%zz", "/%c3%28"] {
            assert_eq!(segments(bad), None, "{bad}");
        }
    }

    #[test]
    fn forms_and_redirects_are_read_safely() {
        assert_eq!(form_value("password=a+b%26c&next=%2Ffilmcraft%2F", "password").as_deref(), Some("a b&c"));
        assert_eq!(form_value("password=x", "next"), None);
        assert_eq!(safe_next(Some("/filmcraft/?webgl")), "/filmcraft/?webgl");
        for bad in [Some("https://evil.example/"), Some("//evil.example/"), Some("/\\evil"), None] {
            assert_eq!(safe_next(bad), "/", "{bad:?}");
        }
        assert_eq!(decode(&encode("/filmcraft/?a=b c"), false).as_deref(), Some("/filmcraft/?a=b c"));
    }

    #[test]
    fn the_sign_in_page_escapes_what_it_echoes() {
        let page = sign_in_page("/\"><script>", Some("<b>"));
        assert!(!page.contains("<script>") && !page.contains("<b>"));
    }
}
