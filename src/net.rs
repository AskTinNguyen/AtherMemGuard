//! HTTP(S) GET through WinHTTP with the system proxy, used by the updater. No TLS crate:
//! Windows does the TLS, follows redirects (HTTPS never drops to HTTP) and finds the proxy.

use std::mem::size_of;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Networking::WinHttp::{
    WinHttpAddRequestHeaders, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetOption, WinHttpSetTimeouts,
    ERROR_WINHTTP_CANNOT_CONNECT, ERROR_WINHTTP_NAME_NOT_RESOLVED, ERROR_WINHTTP_SECURE_FAILURE, ERROR_WINHTTP_TIMEOUT,
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ADDREQ_FLAG_ADD, WINHTTP_ADDREQ_FLAG_REPLACE, WINHTTP_FLAG_SECURE,
    WINHTTP_OPTION_REDIRECT_POLICY, WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP, WINHTTP_QUERY_CONTENT_LENGTH,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
};

use crate::win::wide;

/// The pieces of an http or https URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub https: bool,
    /// Lowercase host name.
    pub host: String,
    pub port: u16,
    /// Path and query, always starting with `/`.
    pub path: String,
}

/// Split an http(s) URL. Rejects other schemes, user names (`user@host`), IPv6 literals and
/// anything with spaces or control characters.
pub fn parse_url(url: &str) -> Option<Url> {
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let (https, rest) = if let Some(r) = strip_prefix_ci(url, "https://") {
        (true, r)
    } else if let Some(r) = strip_prefix_ci(url, "http://") {
        (false, r)
    } else {
        return None;
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    if authority.is_empty() || authority.contains(['@', '[', ']', '\\']) {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().ok().filter(|&p| p != 0)?),
        None => (authority, if https { 443 } else { 80 }),
    };
    if host.is_empty() || !host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
        return None;
    }
    let tail = tail.split('#').next().unwrap_or("");
    let path = if tail.starts_with('/') { tail.to_string() } else { format!("/{}", tail) };
    Some(Url { https, host: host.to_ascii_lowercase(), port, path })
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s.is_char_boundary(prefix.len()) && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

/// True for this PC: `localhost` or `127.0.0.1`.
pub fn is_localhost(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1"
}

struct Internet(*mut core::ffi::c_void);

impl Drop for Internet {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

fn network_error(what: &str) -> String {
    let code = unsafe { GetLastError() };
    match code {
        ERROR_WINHTTP_NAME_NOT_RESOLVED | ERROR_WINHTTP_CANNOT_CONNECT => {
            format!("Can't reach {} (error {}). Check the internet connection.", what, code)
        }
        ERROR_WINHTTP_TIMEOUT => format!("{} took too long to answer (error {}).", capitalize(what), code),
        ERROR_WINHTTP_SECURE_FAILURE => format!("The secure connection to {} failed (error {}).", what, code),
        _ => format!("Can't reach {} (network error {}).", what, code),
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// GET `url` and hand the body to `sink` as it arrives. Only a 200 answer counts; the body
/// may be at most `max_bytes` (checked against Content-Length up front and while reading).
/// `progress(got, total)` runs after every chunk; `total` is 0 when the server doesn't say.
/// `sink` returning false stops the download. Blocking: call it from a worker thread.
pub fn get(
    url: &str,
    max_bytes: u64,
    sink: &mut dyn FnMut(&[u8]) -> bool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<(), String> {
    let u = parse_url(url).ok_or_else(|| format!("{} isn't a web address that can be used.", url))?;
    let what = if is_localhost(&u.host) { "the test server".to_string() } else { u.host.clone() };
    let agent = wide(&format!("AtherMemGuard/{}", env!("CARGO_PKG_VERSION")));
    let host = wide(&u.host);
    let path = wide(&u.path);
    let verb = wide("GET");
    unsafe {
        let session = Internet(WinHttpOpen(agent.as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, null(), null(), 0));
        if session.0.is_null() {
            return Err(network_error(&what));
        }
        let connect = Internet(WinHttpConnect(session.0, host.as_ptr(), u.port, 0));
        if connect.0.is_null() {
            return Err(network_error(&what));
        }
        let flags = if u.https { WINHTTP_FLAG_SECURE } else { 0 };
        let request = Internet(WinHttpOpenRequest(connect.0, verb.as_ptr(), path.as_ptr(), null(), null(), null(), flags));
        if request.0.is_null() {
            return Err(network_error(&what));
        }
        WinHttpSetTimeouts(request.0, 10_000, 15_000, 60_000, 60_000);
        let policy: u32 = WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP;
        WinHttpSetOption(
            request.0,
            WINHTTP_OPTION_REDIRECT_POLICY,
            &policy as *const u32 as *const core::ffi::c_void,
            size_of::<u32>() as u32,
        );
        let headers = wide("Cache-Control: no-cache");
        WinHttpAddRequestHeaders(request.0, headers.as_ptr(), u32::MAX, WINHTTP_ADDREQ_FLAG_ADD | WINHTTP_ADDREQ_FLAG_REPLACE);
        if WinHttpSendRequest(request.0, null(), 0, null(), 0, 0, 0) == 0 || WinHttpReceiveResponse(request.0, null_mut()) == 0 {
            return Err(network_error(&what));
        }

        let mut status: u32 = 0;
        let mut size = size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            &mut status as *mut u32 as *mut core::ffi::c_void,
            &mut size,
            null_mut(),
        );
        if status != 200 {
            return Err(format!("{} answered {} instead of sending the file.", capitalize(&what), status));
        }
        let mut len_buf = [0u16; 32];
        let mut size = (len_buf.len() * 2) as u32;
        let total = if WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_CONTENT_LENGTH,
            null(),
            len_buf.as_mut_ptr() as *mut core::ffi::c_void,
            &mut size,
            null_mut(),
        ) != 0
        {
            let n = (size as usize / 2).min(len_buf.len());
            String::from_utf16_lossy(&len_buf[..n]).trim().parse::<u64>().unwrap_or(0)
        } else {
            0
        };
        if total > max_bytes {
            return Err("The file is larger than expected, so it wasn't downloaded.".into());
        }

        let mut buf = vec![0u8; 64 * 1024];
        let mut got: u64 = 0;
        loop {
            let mut n: u32 = 0;
            if WinHttpReadData(request.0, buf.as_mut_ptr() as *mut core::ffi::c_void, buf.len() as u32, &mut n) == 0 {
                return Err("The download was interrupted.".into());
            }
            if n == 0 {
                break;
            }
            got += n as u64;
            if got > max_bytes {
                return Err("The file is larger than expected, so it wasn't downloaded.".into());
            }
            if !sink(&buf[..n as usize]) {
                return Err("The download couldn't be saved.".into());
            }
            progress(got, total);
        }
        if total != 0 && got != total {
            return Err("The download was interrupted.".into());
        }
    }
    Ok(())
}

/// GET a small body into memory (at most `max_bytes`).
pub fn get_bytes(url: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    get(
        url,
        max_bytes,
        &mut |chunk| {
            body.extend_from_slice(chunk);
            true
        },
        &mut |_, _| {},
    )?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let u = parse_url("https://GitHub.com/AskTinNguyen/AtherMemGuard/releases/latest/download/latest.json").unwrap();
        assert!(u.https);
        assert_eq!(u.host, "github.com");
        assert_eq!(u.port, 443);
        assert_eq!(u.path, "/AskTinNguyen/AtherMemGuard/releases/latest/download/latest.json");

        let u = parse_url("http://127.0.0.1:8123/latest.json?x=1#frag").unwrap();
        assert!(!u.https);
        assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("127.0.0.1", 8123, "/latest.json?x=1"));

        let u = parse_url("HTTP://localhost").unwrap();
        assert_eq!((u.port, u.path.as_str()), (80, "/"));
        assert_eq!(parse_url("https://example.com?q").unwrap().path, "/?q");
    }

    #[test]
    fn rejects_bad_urls() {
        for bad in [
            "ftp://github.com/x",
            "github.com/x",
            "https://",
            "https:///x",
            "https://user@github.com/x",
            "https://github.com:0/x",
            "https://github.com:99999/x",
            "https://[::1]/x",
            "https://git hub.com/x",
            "https://github.com/a b",
            "https://evil.com\\@github.com/",
            "https://exa_mple.com/",
        ] {
            assert!(parse_url(bad).is_none(), "{} should be rejected", bad);
        }
    }

    #[test]
    fn knows_localhost() {
        assert!(is_localhost("localhost"));
        assert!(is_localhost("LocalHost"));
        assert!(is_localhost("127.0.0.1"));
        assert!(!is_localhost("127.0.0.2"));
        assert!(!is_localhost("localhost.evil.com"));
    }
}
