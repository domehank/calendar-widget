//! Google side of the calendar widget: HTTPS through WinHTTP (Windows' own TLS, no extra
//! crates), the OAuth "installed app" loopback sign-in, the refresh token stored encrypted with
//! DPAPI (only this Windows user can decrypt it), and the Calendar API events query.
//!
//! Files in ~/.claude/calendar-widget/:
//!   client_secret*.json  OAuth "Desktop app" client downloaded from Google Cloud Console
//!   token.bin            refresh token, DPAPI-encrypted

use std::io::{Read, Write};
use std::mem::zeroed;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Networking::WinHttp::*;
use windows_sys::Win32::Security::Cryptography::*;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const SCOPE: &str = "https://www.googleapis.com/auth/calendar.readonly";
/// How far ahead events are fetched (the bar shows one hour; the tooltip lists the rest).
pub const LOOKAHEAD_SECS: i64 = 6 * 3600;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Opens a URL or folder with the default handler. ShellExecute needs COM initialized on the
/// calling thread (without it, it can crash inside combase.dll), so it runs on its own
/// short-lived STA thread; that also keeps a slow browser launch off the UI thread.
pub fn open_url(url: &str) {
    let url = wide(url);
    std::thread::spawn(move || unsafe {
        use windows_sys::Win32::System::Com::*;
        let hr = CoInitializeEx(null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32);
        ShellExecuteW(null_mut(), wide("open").as_ptr(), url.as_ptr(), null(), null(), SW_SHOWNORMAL);
        if hr >= 0 {
            CoUninitialize();
        }
    });
}

// ---- HTTPS --------------------------------------------------------------------------------

struct Handle(*mut core::ffi::c_void);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

struct Resp {
    status: u32,
    body: String,
}

fn net_error(code: u32) -> String {
    match code {
        12002 => "request timed out".into(),
        12007 | 12029 => "offline (cannot reach Google)".into(),
        _ => format!("network error {code}"),
    }
}

fn https(method: &str, host: &str, path: &str, headers: &str, body: &[u8]) -> Result<Resp, String> {
    unsafe {
        let session = Handle(WinHttpOpen(
            wide("CalendarTaskbarWidget/1.0").as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            null(),
            null(),
            0,
        ));
        if session.0.is_null() {
            return Err(net_error(GetLastError()));
        }
        WinHttpSetTimeouts(session.0, 10_000, 10_000, 15_000, 20_000);
        let conn = Handle(WinHttpConnect(session.0, wide(host).as_ptr(), 443, 0));
        if conn.0.is_null() {
            return Err(net_error(GetLastError()));
        }
        let req = Handle(WinHttpOpenRequest(
            conn.0,
            wide(method).as_ptr(),
            wide(path).as_ptr(),
            null(),
            null(),
            null(),
            WINHTTP_FLAG_SECURE,
        ));
        if req.0.is_null() {
            return Err(net_error(GetLastError()));
        }
        let h = wide(headers);
        let (hp, hl) = if headers.is_empty() { (null(), 0) } else { (h.as_ptr(), u32::MAX) };
        let bp = if body.is_empty() { null() } else { body.as_ptr() as *const _ };
        let n = body.len() as u32;
        if WinHttpSendRequest(req.0, hp, hl, bp, n, n, 0) == 0 || WinHttpReceiveResponse(req.0, null_mut()) == 0 {
            return Err(net_error(GetLastError()));
        }
        let mut status = 0u32;
        let mut size = 4u32;
        WinHttpQueryHeaders(
            req.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            &mut status as *mut u32 as *mut _,
            &mut size,
            null_mut(),
        );
        let mut out: Vec<u8> = Vec::new();
        loop {
            let mut avail = 0u32;
            if WinHttpQueryDataAvailable(req.0, &mut avail) == 0 {
                return Err(net_error(GetLastError()));
            }
            if avail == 0 {
                break;
            }
            let start = out.len();
            out.resize(start + avail as usize, 0);
            let mut read = 0u32;
            if WinHttpReadData(req.0, out[start..].as_mut_ptr() as *mut _, avail, &mut read) == 0 {
                return Err(net_error(GetLastError()));
            }
            out.truncate(start + read as usize);
        }
        Ok(Resp { status, body: String::from_utf8_lossy(&out).into_owned() })
    }
}

fn json(r: &Resp) -> Value {
    serde_json::from_str(&r.body).unwrap_or(Value::Null)
}

fn api_error(what: &str, r: &Resp) -> String {
    let v = json(r);
    let msg = v["error"]["message"]
        .as_str()
        .or_else(|| v["error_description"].as_str())
        .or_else(|| v["error"].as_str())
        .unwrap_or("");
    format!("{what}: HTTP {} {msg}", r.status).trim_end().to_string()
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn dec(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---- secrets ------------------------------------------------------------------------------

/// DPAPI encrypt (decrypt = false) or decrypt for the current Windows user.
fn dpapi(data: &[u8], decrypt: bool) -> Option<Vec<u8>> {
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out: CRYPT_INTEGER_BLOB = zeroed();
        let ok = if decrypt {
            CryptUnprotectData(&input, null_mut(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        } else {
            let desc = wide("Calendar widget Google token");
            CryptProtectData(&input, desc.as_ptr(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        };
        if ok == 0 {
            return None;
        }
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as HLOCAL);
        Some(v)
    }
}

fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    unsafe { BCryptGenRandom(null_mut(), b.as_mut_ptr(), n as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    b.iter().map(|x| format!("{x:02x}")).collect()
}

struct Client {
    id: String,
    secret: String,
}

/// Reads the OAuth client JSON (Google names it client_secret_<id>.apps.googleusercontent.com.json).
fn load_client(dir: &Path) -> Option<Client> {
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().to_lowercase();
        if !(name.starts_with("client_secret") && name.ends_with(".json")) {
            continue;
        }
        let Ok(s) = std::fs::read_to_string(e.path()) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(s.trim_start_matches('\u{feff}')) else { continue };
        let c = if v["installed"].is_object() { &v["installed"] } else { &v["web"] };
        if let (Some(id), Some(secret)) = (c["client_id"].as_str(), c["client_secret"].as_str()) {
            return Some(Client { id: id.into(), secret: secret.into() });
        }
    }
    None
}

pub fn has_client(dir: &Path) -> bool {
    load_client(dir).is_some()
}

fn token_file(dir: &Path) -> PathBuf {
    dir.join("token.bin")
}

fn load_refresh_token(dir: &Path) -> Option<String> {
    let raw = dpapi(&std::fs::read(token_file(dir)).ok()?, true)?;
    let v: Value = serde_json::from_slice(&raw).ok()?;
    v["refresh_token"].as_str().map(String::from)
}

fn save_refresh_token(dir: &Path, token: &str) -> Result<(), String> {
    let raw = serde_json::json!({ "refresh_token": token }).to_string();
    let blob = dpapi(raw.as_bytes(), false).ok_or("could not encrypt the token")?;
    std::fs::write(token_file(dir), blob).map_err(|e| format!("could not save the token: {e}"))
}

pub fn has_token(dir: &Path) -> bool {
    token_file(dir).exists()
}

/// (access token, expiry), shared by the fetch threads
static ACCESS: Mutex<Option<(String, i64)>> = Mutex::new(None);
static CAL_COLOR: Mutex<Option<u32>> = Mutex::new(None);

pub fn sign_out(dir: &Path) {
    let _ = std::fs::remove_file(token_file(dir));
    *ACCESS.lock().unwrap() = None;
}

fn remember_access(v: &Value) -> Option<String> {
    let at = v["access_token"].as_str()?.to_string();
    let exp = now() + v["expires_in"].as_i64().unwrap_or(3600);
    *ACCESS.lock().unwrap() = Some((at.clone(), exp));
    Some(at)
}

// ---- sign-in ------------------------------------------------------------------------------

fn respond(sock: &mut std::net::TcpStream, status: &str, msg: &str) {
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>Calendar widget</title>\
         <body style=\"font:16px Segoe UI,sans-serif;padding:40px\"><h2>{msg}</h2>"
    );
    let _ = write!(
        sock,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
}

/// Opens the Google consent page in the browser and waits (up to 5 minutes) for the redirect
/// to a local port, then trades the code for a refresh token.
pub fn sign_in(dir: &Path) -> Result<(), String> {
    let client = load_client(dir).ok_or("no client_secret*.json in the widget folder")?;
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let redirect = format!("http://127.0.0.1:{}", listener.local_addr().map_err(|e| e.to_string())?.port());
    let state = random_hex(16);
    open_url(&format!(
        "https://accounts.google.com/o/oauth2/v2/auth?client_id={}&redirect_uri={}&response_type=code\
         &scope={}&access_type=offline&prompt=consent&state={}",
        enc(&client.id),
        enc(&redirect),
        enc(SCOPE),
        state
    ));

    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let mut sock = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() > deadline {
                    return Err("sign-in timed out".into());
                }
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            Err(e) => return Err(e.to_string()),
        };
        let _ = sock.set_nonblocking(false);
        let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buf = vec![0u8; 8192];
        let mut len = 0;
        while len < buf.len() {
            match sock.read(&mut buf[len..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    len += n;
                    if buf[..len].windows(2).any(|w| w == b"\r\n") {
                        break;
                    }
                }
            }
        }
        let req = String::from_utf8_lossy(&buf[..len]).into_owned();
        let target = req.split_whitespace().nth(1).unwrap_or("");
        let Some(query) = target.strip_prefix("/?") else {
            respond(&mut sock, "404 Not Found", "Not found");
            continue; // e.g. /favicon.ico
        };
        let param = |k: &str| {
            query.split('&').find_map(|kv| kv.strip_prefix(&format!("{k}=")).map(dec))
        };
        if param("state").as_deref() != Some(state.as_str()) {
            respond(&mut sock, "400 Bad Request", "Unexpected sign-in response; try again from the widget.");
            continue;
        }
        if let Some(err) = param("error") {
            respond(&mut sock, "200 OK", "Sign-in cancelled. You can close this tab.");
            return Err(format!("sign-in cancelled ({err})"));
        }
        let code = param("code").ok_or("no code in the sign-in response")?;

        let body = format!(
            "code={}&client_id={}&client_secret={}&redirect_uri={}&grant_type=authorization_code",
            enc(&code),
            enc(&client.id),
            enc(&client.secret),
            enc(&redirect)
        );
        let result = https("POST", "oauth2.googleapis.com", "/token", "Content-Type: application/x-www-form-urlencoded", body.as_bytes())
            .and_then(|r| {
                let v = json(&r);
                match (r.status, v["refresh_token"].as_str()) {
                    (200, Some(rt)) => {
                        remember_access(&v);
                        save_refresh_token(dir, rt)
                    }
                    (200, None) => Err("Google returned no refresh token".into()),
                    _ => Err(api_error("token exchange", &r)),
                }
            });
        match &result {
            Ok(()) => respond(&mut sock, "200 OK", "Signed in. You can close this tab."),
            Err(e) => respond(&mut sock, "200 OK", &format!("Sign-in failed: {e}")),
        }
        return result;
    }
}

// ---- events -------------------------------------------------------------------------------

pub struct Event {
    pub title: String,
    pub start: i64,
    pub end: i64,
    pub color: (u8, u8, u8),
    pub tentative: bool,
    pub link: String,         // event page in Google Calendar
    pub meet: Option<String>, // Google Meet link, if any
}

pub enum FetchError {
    NoClient,
    SignedOut,
    Failed(String),
}

fn access_token(dir: &Path, client: &Client) -> Result<String, FetchError> {
    if let Some((at, exp)) = ACCESS.lock().unwrap().clone() {
        if exp > now() + 60 {
            return Ok(at);
        }
    }
    let rt = load_refresh_token(dir).ok_or(FetchError::SignedOut)?;
    let body = format!(
        "client_id={}&client_secret={}&refresh_token={}&grant_type=refresh_token",
        enc(&client.id),
        enc(&client.secret),
        enc(&rt)
    );
    let r = https("POST", "oauth2.googleapis.com", "/token", "Content-Type: application/x-www-form-urlencoded", body.as_bytes())
        .map_err(FetchError::Failed)?;
    let v = json(&r);
    if v["error"].as_str() == Some("invalid_grant") {
        sign_out(dir); // revoked or expired: the user has to sign in again
        return Err(FetchError::SignedOut);
    }
    if r.status != 200 {
        return Err(FetchError::Failed(api_error("token refresh", &r)));
    }
    remember_access(&v).ok_or_else(|| FetchError::Failed("token refresh: no access token".into()))
}

fn hex_color(s: &str) -> Option<(u8, u8, u8)> {
    let s = s.strip_prefix('#')?;
    let p = |i: usize| u8::from_str_radix(s.get(i..i + 2)?, 16).ok();
    Some((p(0)?, p(2)?, p(4)?))
}

/// Google Calendar's event color palette (colorId 1-11).
fn event_color(id: &str) -> Option<(u8, u8, u8)> {
    Some(match id {
        "1" => (121, 134, 203),
        "2" => (51, 182, 121),
        "3" => (142, 36, 170),
        "4" => (230, 124, 115),
        "5" => (246, 191, 38),
        "6" => (244, 81, 30),
        "7" => (3, 155, 229),
        "8" => (97, 97, 97),
        "9" => (63, 81, 181),
        "10" => (11, 128, 67),
        "11" => (213, 0, 0),
        _ => return None,
    })
}

fn get(path: &str, at: &str) -> Result<Resp, FetchError> {
    let r = https("GET", "www.googleapis.com", path, &format!("Authorization: Bearer {at}"), &[]).map_err(FetchError::Failed)?;
    if r.status == 401 {
        *ACCESS.lock().unwrap() = None; // retried on the next refresh with a new access token
    }
    Ok(r)
}

/// Events on the primary calendar from now to now + LOOKAHEAD_SECS, recurring events expanded,
/// all-day events, working-location entries and declined invitations left out.
pub fn fetch(dir: &Path) -> Result<Vec<Event>, FetchError> {
    let client = load_client(dir).ok_or(FetchError::NoClient)?;
    let at = access_token(dir, &client)?;

    if CAL_COLOR.lock().unwrap().is_none() {
        let r = get("/calendar/v3/users/me/calendarList/primary?fields=backgroundColor", &at)?;
        if let Some(c) = json(&r)["backgroundColor"].as_str().and_then(hex_color) {
            *CAL_COLOR.lock().unwrap() = Some((c.0 as u32) << 16 | (c.1 as u32) << 8 | c.2 as u32);
        }
    }
    let default_color = CAL_COLOR
        .lock()
        .unwrap()
        .map(|c| ((c >> 16) as u8, (c >> 8) as u8, c as u8))
        .unwrap_or((3, 155, 229));

    let t = now();
    let fields = "items(summary,start,end,colorId,htmlLink,hangoutLink,eventType,status,attendees(self,responseStatus))";
    let path = format!(
        "/calendar/v3/calendars/primary/events?singleEvents=true&orderBy=startTime&maxResults=100\
         &timeMin={}&timeMax={}&fields={}",
        enc(&rfc3339(t - 15 * 60)), // ended events can still fade out on the left
        enc(&rfc3339(t + LOOKAHEAD_SECS)),
        enc(fields)
    );
    let r = get(&path, &at)?;
    if r.status != 200 {
        return Err(FetchError::Failed(api_error("Calendar API", &r)));
    }
    let v = json(&r);
    let mut out = Vec::new();
    for e in v["items"].as_array().into_iter().flatten() {
        if e["status"] == "cancelled" || matches!(e["eventType"].as_str(), Some("workingLocation" | "birthday")) {
            continue;
        }
        // all-day events have start.date instead of start.dateTime
        let (Some(start), Some(end)) = (
            e["start"]["dateTime"].as_str().and_then(parse_rfc3339),
            e["end"]["dateTime"].as_str().and_then(parse_rfc3339),
        ) else {
            continue;
        };
        let me = e["attendees"].as_array().and_then(|a| a.iter().find(|x| x["self"] == true));
        let response = me.and_then(|m| m["responseStatus"].as_str()).unwrap_or("accepted");
        if response == "declined" {
            continue;
        }
        out.push(Event {
            title: e["summary"].as_str().unwrap_or("(No title)").to_string(),
            start,
            end,
            color: e["colorId"].as_str().and_then(event_color).unwrap_or(default_color),
            tentative: matches!(response, "tentative" | "needsAction"),
            link: e["htmlLink"].as_str().unwrap_or("").to_string(),
            meet: e["hangoutLink"].as_str().map(String::from),
        });
    }
    Ok(out)
}

// ---- time ---------------------------------------------------------------------------------

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

/// "2026-11-14T16:00:00+01:00" / "...Z" / with fractional seconds -> unix seconds
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let n = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let days = days_from_civil(n(0, 4)?, n(5, 7)?, n(8, 10)?);
    let secs = days * 86_400 + n(11, 13)? * 3600 + n(14, 16)? * 60 + n(17, 19)?;
    let mut rest = s.get(19..)?;
    if let Some(r) = rest.strip_prefix('.') {
        rest = r.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let offset = match rest.chars().next() {
        Some('Z' | 'z') | None => 0,
        Some(sign @ ('+' | '-')) => {
            let h: i64 = rest.get(1..3)?.parse().ok()?;
            let m: i64 = rest.get(4..6)?.parse().ok()?;
            (h * 3600 + m * 60) * if sign == '-' { -1 } else { 1 }
        }
        _ => return None,
    };
    Some(secs - offset)
}

fn rfc3339(unix: i64) -> String {
    let (y, m, d) = civil_from_days(unix.div_euclid(86_400));
    let s = unix.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", s / 3600, s / 60 % 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_roundtrip() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-11-14T16:00:00+01:00"), Some(1_794_668_400));
        assert_eq!(parse_rfc3339("2026-11-14T15:00:00.000Z"), Some(1_794_668_400));
        assert_eq!(parse_rfc3339("2026-11-14T10:00:00-05:00"), Some(1_794_668_400));
        assert_eq!(rfc3339(1_794_668_400), "2026-11-14T15:00:00Z");
    }

    #[test]
    fn url_coding() {
        assert_eq!(dec(&enc("a b/c?d=é")), "a b/c?d=é");
        assert_eq!(dec("4%2F0Ab+x"), "4/0Ab x");
    }
}
