//! The self-contained HTML page (inline CSS, no scripts, nothing external) that the RAM and
//! BIOS check and the drivers report open in the browser, in the Ather look.

/// A whole page titled `title` with `body` (the content of `<main>`) and the shared style.
pub fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>{}</title>\n<style>\n{}</style>\n</head>\n<body>\n<main>\n{}</main>\n</body>\n</html>\n",
        esc(title),
        CSS,
        body
    )
}

/// Text for HTML: markup characters escaped, control characters dropped.
pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c if c.is_control() => {}
            c => o.push(c),
        }
    }
    o
}

/// `esc`, with `backticked` spans as `<code>`.
pub fn esc_rich(s: &str) -> String {
    let e = esc(s);
    let mut o = String::new();
    for (i, part) in e.split('`').enumerate() {
        if i % 2 == 1 {
            o.push_str(&format!("<code>{}</code>", part));
        } else {
            o.push_str(part);
        }
    }
    o
}

const CSS: &str = r#":root { color-scheme: dark; }
* { box-sizing: border-box; }
body { margin: 0; background: rgb(14,14,13); color: rgb(242,239,232);
  font: 15px/1.55 "Segoe UI Variable Text", "Segoe UI", system-ui, sans-serif; }
main { max-width: 860px; margin: 0 auto; padding: 32px 16px 48px; }
.eyebrow { font-family: "Bahnschrift SemiBold", "Bahnschrift", "Segoe UI", sans-serif; text-transform: uppercase;
  letter-spacing: 3px; font-size: 12px; color: rgb(212,255,0); margin: 0 0 14px; }
h1 { font-family: "Bahnschrift SemiBold Condensed", "Bahnschrift Condensed", "Bahnschrift", "Segoe UI", sans-serif;
  font-stretch: condensed; text-transform: uppercase; font-weight: 600; font-size: 34px; line-height: 1.1; margin: 8px 0 12px; }
h2 { font-family: "Bahnschrift SemiBold", "Bahnschrift", "Segoe UI", sans-serif; text-transform: uppercase;
  letter-spacing: 3px; font-size: 13px; color: rgb(212,255,0); margin: 0 0 14px; font-weight: 600; }
.summary { color: rgb(208,206,199); margin: 0 0 24px; max-width: 70ch; }
.pill { display: inline-block; margin: 0; padding: 3px 10px; border-radius: 6px; font-size: 12px; font-weight: 600;
  text-transform: uppercase; letter-spacing: 1px; }
.pill.ok { background: rgb(212,255,0); color: rgb(14,14,13); }
.pill.warn { background: rgb(255,184,48); color: rgb(14,14,13); }
.pill.bad { background: rgb(255,120,110); color: rgb(14,14,13); }
.card { background: rgb(26,27,25); border: 1px solid rgb(50,52,46); border-radius: 8px; padding: 20px; margin: 0 0 16px; }
.scroll { overflow-x: auto; }
table { width: 100%; border-collapse: collapse; font-size: 14px; }
th { text-align: left; color: rgb(150,150,142); font-weight: 600; padding: 6px 10px 6px 0; border-bottom: 1px solid rgb(50,52,46); white-space: nowrap; }
td { padding: 7px 10px 7px 0; border-bottom: 1px solid rgb(37,38,35); vertical-align: top; }
tr.empty td { color: rgb(150,150,142); }
table.nums td:nth-child(2), table.nums td:nth-child(3) { white-space: nowrap; }
.mono, code { font-family: "Cascadia Mono", Consolas, monospace; font-size: 13px; }
code { background: rgb(37,38,35); padding: 1px 5px; border-radius: 4px; }
dl { display: grid; grid-template-columns: max-content 1fr; gap: 6px 16px; margin: 16px 0 0; }
dt { color: rgb(150,150,142); }
dd { margin: 0; }
.findings { list-style: none; padding: 0; margin: 0; }
.findings li { border-left: 3px solid rgb(150,150,142); padding: 2px 0 2px 14px; margin: 0 0 16px; }
.findings li.problem { border-color: rgb(255,120,110); }
.findings li.caution { border-color: rgb(255,184,48); }
.findings li.info { border-color: rgb(212,255,0); }
.findings p { margin: 4px 0 0; color: rgb(208,206,199); overflow-wrap: anywhere; }
.tag { display: inline-block; font-size: 11px; text-transform: uppercase; letter-spacing: 1px; margin-right: 8px; color: rgb(150,150,142); }
li.problem .tag { color: rgb(255,120,110); }
li.caution .tag { color: rgb(255,184,48); }
.steps { margin: 0; padding-left: 22px; }
.steps li { margin: 0 0 10px; padding-left: 4px; }
.steps li::marker { color: rgb(212,255,0); font-weight: 700; }
.muted { color: rgb(150,150,142); margin: -6px 0 14px; }
a { color: rgb(212,255,0); }
.foot { color: rgb(150,150,142); font-size: 13px; margin-top: 24px; }
@media (max-width: 560px) { h1 { font-size: 26px; } dl { grid-template-columns: 1fr; } dt { margin-top: 6px; } }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_wraps_body_and_escapes() {
        let p = page("A < B", "<p>hi</p>\n");
        assert!(p.starts_with("<!doctype html>") && p.contains("<title>A &lt; B</title>") && p.contains("<main>\n<p>hi</p>\n</main>"));
        assert_eq!(esc_rich("run `memguard` & see"), "run <code>memguard</code> &amp; see");
    }
}
