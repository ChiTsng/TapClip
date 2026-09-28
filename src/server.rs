//! Minimal LAN HTTP endpoint for the iPhone shortcut.
//!
//! One connection at a time, strict size limits and timeouts, token auth with
//! a per-IP lockout. Endpoints: `GET /ping`, `POST /tap`.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Cursor, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::bridge::{TapError, Tapper};
use crate::config::{Config, normalize_token};
use crate::engine::{Choice, MAX_TEXT_BYTES, TapOutcome};
use crate::log;

const MAX_HEAD: usize = 16 * 1024;
/// Text limit plus JSON escaping overhead.
const MAX_BODY: usize = MAX_TEXT_BYTES * 2;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const LOCKOUT_FAILURES: u32 = 10;
const LOCKOUT_WINDOW: Duration = Duration::from_secs(600);

pub fn spawn<T: Tapper + Send + 'static>(config: &Config, tapper: T) -> io::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", config.port))?;
    let mut server = Server::new(config, tapper);
    thread::Builder::new().name("tapclip-http".into()).spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => server.handle(stream),
                Err(e) => log!("accept failed: {e}"),
            }
        }
    })?;
    Ok(())
}

pub struct Server<T> {
    token: String,
    ttl_seconds: u64,
    tapper: T,
    lockout: Lockout,
}

impl<T: Tapper> Server<T> {
    pub fn new(config: &Config, tapper: T) -> Self {
        Self {
            token: normalize_token(&config.token),
            ttl_seconds: config.ttl_seconds,
            tapper,
            lockout: Lockout::default(),
        }
    }

    fn handle(&mut self, mut stream: TcpStream) {
        let peer = stream.peer_addr().map(|a| a.ip()).ok();
        let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
        let _ = stream.set_nodelay(true);
        let response = match read_request(&mut stream) {
            Ok(request) => self.respond(peer, &request, Instant::now()),
            Err(e) => e.response(),
        };
        let _ = stream.write_all(&response.to_bytes());
    }

    pub fn respond(&mut self, peer: Option<IpAddr>, req: &Request, now: Instant) -> Response {
        if !matches!((req.method.as_str(), req.path.as_str()), ("GET", "/ping") | ("POST", "/tap")) {
            return Response::message(404, "没有这个地址");
        }
        if let Some(ip) = peer
            && self.lockout.is_locked(ip, now)
        {
            return Response::message(429, "密钥错误次数过多，请 10 分钟后再试");
        }
        if !self.authorized(req) {
            if let Some(ip) = peer {
                self.lockout.fail(ip, now);
            }
            log!("rejected request from {}: bad token", peer.map_or_else(|| "unknown".into(), |ip| ip.to_string()));
            return Response::message(401, "密钥不对，请检查快捷指令里的密钥");
        }
        if let Some(ip) = peer {
            self.lockout.clear(ip);
        }

        match req.path.as_str() {
            "/ping" => Response::message(200, "TapClip 已连接"),
            _ => self.tap(&req.body),
        }
    }

    fn authorized(&self, req: &Request) -> bool {
        let given = req
            .header("authorization")
            .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
            .or_else(|| req.header("x-tapclip-token"));
        given.is_some_and(|t| constant_time_eq(normalize_token(t).as_bytes(), self.token.as_bytes()))
    }

    fn tap(&mut self, body: &[u8]) -> Response {
        let tap: TapRequest = if body.iter().all(u8::is_ascii_whitespace) {
            TapRequest::default()
        } else {
            match serde_json::from_slice(body) {
                Ok(tap) => tap,
                Err(_) => return Response::message(400, "请求格式不对（需要 JSON）"),
            }
        };
        if tap.text.as_ref().is_some_and(|t| t.len() > MAX_TEXT_BYTES) {
            return Response::message(413, "内容太大，TapClip 只同步 1 MB 以内的文字");
        }
        let choice = match tap.choice.as_deref().map(str::trim) {
            Some("send") => Choice::Send,
            Some("receive") => Choice::Receive,
            _ => Choice::Auto,
        };
        let phone_len = tap.text.as_ref().map_or(0, String::len);
        let result = self.tapper.tap(tap.text, choice);
        log!("tap choice={choice:?} phone_len={phone_len} -> {}", describe(&result));
        Response::json(200, &self.reply(result))
    }

    fn reply(&self, result: Result<TapOutcome, TapError>) -> TapReply {
        let ttl = human_duration(self.ttl_seconds);
        match result {
            Ok(TapOutcome::Sent) => TapReply::say(format!("⬆ 已发送到电脑，{ttl}内粘贴有效")),
            Ok(TapOutcome::Received(text)) => {
                TapReply {
                    message: format!("⬇ 已从电脑取来：{}", preview(&text)), copy: Some(text), ask: None
                }
            }
            Ok(TapOutcome::Ask) => {
                TapReply {
                    message: "两边都有新内容，请选择方向".into(), copy: None, ask: Some("1".into())
                }
            }
            Ok(TapOutcome::InSync) => TapReply::say("两边已经是同一段内容".into()),
            Ok(TapOutcome::Nothing) => TapReply::say("没有新内容".into()),
            Err(TapError::Busy) => TapReply::say("电脑剪贴板正忙，请再碰一次".into()),
            Err(TapError::Unavailable) => TapReply::say("电脑没有响应，请再碰一次".into()),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct TapRequest {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    choice: Option<String>,
}

/// Response for the shortcut. Optional fields are omitted so the shortcut can
/// test them with "has any value".
#[derive(Debug, Serialize)]
struct TapReply {
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    copy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ask: Option<String>,
}

impl TapReply {
    fn say(message: String) -> Self {
        Self { message, copy: None, ask: None }
    }
}

fn describe(result: &Result<TapOutcome, TapError>) -> String {
    match result {
        Ok(TapOutcome::Received(text)) => format!("Received(len={})", text.len()),
        other => format!("{other:?}"),
    }
}

fn human_duration(seconds: u64) -> String {
    if seconds.is_multiple_of(60) { format!("{} 分钟", seconds / 60) } else { format!("{seconds} 秒") }
}

/// First line, at most 24 characters.
fn preview(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or_default();
    let mut chars = line.chars();
    let head: String = chars.by_ref().take(24).collect();
    if chars.next().is_some() || text.trim().lines().nth(1).is_some() { format!("{head}…") } else { head }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Default)]
struct Lockout {
    failures: HashMap<IpAddr, (u32, Instant)>,
}

impl Lockout {
    fn is_locked(&self, ip: IpAddr, now: Instant) -> bool {
        self.failures
            .get(&ip)
            .is_some_and(|&(n, since)| n >= LOCKOUT_FAILURES && now.saturating_duration_since(since) < LOCKOUT_WINDOW)
    }

    fn fail(&mut self, ip: IpAddr, now: Instant) {
        if self.failures.len() > 1024 {
            self.failures.retain(|_, (_, since)| now.saturating_duration_since(*since) < LOCKOUT_WINDOW);
        }
        let entry = self.failures.entry(ip).or_insert((0, now));
        if now.saturating_duration_since(entry.1) >= LOCKOUT_WINDOW {
            *entry = (0, now);
        }
        entry.0 += 1;
    }

    fn clear(&mut self, ip: IpAddr) {
        self.failures.remove(&ip);
    }
}

// ---------------------------------------------------------------------------
// HTTP/1.1 request parsing and response writing

#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// Lower-cased names.
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

#[derive(Debug)]
pub struct HttpError(u16, &'static str);

impl HttpError {
    fn response(&self) -> Response {
        Response::message(self.0, self.1)
    }
}

impl From<io::Error> for HttpError {
    fn from(_: io::Error) -> Self {
        HttpError(400, "连接中断或超时")
    }
}

pub fn read_request<R: Read>(stream: &mut R) -> Result<Request, HttpError> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];
    let (mut request, head_len) = loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(HttpError(400, "请求不完整"));
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(parsed) = parse_head(&buf)? {
            break parsed;
        }
        if buf.len() > MAX_HEAD {
            return Err(HttpError(431, "请求头太大"));
        }
    };

    let leftover = buf.split_off(head_len);
    let mut reader = BufReader::new(Cursor::new(leftover).chain(stream));
    let chunked = request.header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    request.body = if chunked {
        read_chunked(&mut reader)?
    } else {
        let len: usize = match request.header("content-length") {
            Some(v) => v.trim().parse().map_err(|_| HttpError(400, "Content-Length 无效"))?,
            None => 0,
        };
        if len > MAX_BODY {
            return Err(HttpError(413, "内容太大"));
        }
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body)?;
        body
    };
    Ok(request)
}

fn parse_head(buf: &[u8]) -> Result<Option<(Request, usize)>, HttpError> {
    let mut headers = [httparse::EMPTY_HEADER; 48];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(buf) {
        Ok(httparse::Status::Complete(len)) => {
            let path = req.path.unwrap_or("/");
            let request = Request {
                method: req.method.unwrap_or_default().to_owned(),
                path: path.split('?').next().unwrap_or("/").to_owned(),
                headers: req
                    .headers
                    .iter()
                    .map(|h| (h.name.to_ascii_lowercase(), String::from_utf8_lossy(h.value).trim().to_owned()))
                    .collect(),
                body: Vec::new(),
            };
            Ok(Some((request, len)))
        }
        Ok(httparse::Status::Partial) => Ok(None),
        Err(httparse::Error::TooManyHeaders) => Err(HttpError(431, "请求头太多")),
        Err(_) => Err(HttpError(400, "请求格式不对")),
    }
}

fn read_chunked<R: BufRead>(reader: &mut R) -> Result<Vec<u8>, HttpError> {
    let mut body = Vec::new();
    loop {
        let line = read_line(reader)?;
        let size_hex = line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_hex, 16).map_err(|_| HttpError(400, "分块长度无效"))?;
        if size == 0 {
            // Skip optional trailers up to the terminating blank line.
            while !read_line(reader)?.is_empty() {}
            return Ok(body);
        }
        if body.len() + size > MAX_BODY {
            return Err(HttpError(413, "内容太大"));
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..])?;
        if !read_line(reader)?.is_empty() {
            return Err(HttpError(400, "分块格式不对"));
        }
    }
}

/// Reads one CRLF-terminated line of at most 256 bytes, without the CRLF.
fn read_line<R: BufRead>(reader: &mut R) -> Result<String, HttpError> {
    let mut line = Vec::new();
    reader.take(258).read_until(b'\n', &mut line)?;
    if line.last() != Some(&b'\n') {
        return Err(HttpError(400, "行太长或不完整"));
    }
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    String::from_utf8(line).map_err(|_| HttpError(400, "请求格式不对"))
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

impl Response {
    fn json<S: Serialize>(status: u16, value: &S) -> Self {
        Self { status, body: serde_json::to_string(value).unwrap_or_else(|_| "{}".into()) }
    }

    fn message(status: u16, message: &str) -> Self {
        Self::json(status, &TapReply::say(message.to_owned()))
    }

    fn to_bytes(&self) -> Vec<u8> {
        let reason = match self.status {
            200 => "OK",
            400 => "Bad Request",
            401 => "Unauthorized",
            404 => "Not Found",
            413 => "Payload Too Large",
            429 => "Too Many Requests",
            431 => "Request Header Fields Too Large",
            _ => "Error",
        };
        let mut out = format!(
            "HTTP/1.1 {} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
            self.status,
            self.body.len()
        )
        .into_bytes();
        out.extend_from_slice(self.body.as_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::net::Ipv4Addr;

    struct FakeTapper {
        outcome: TapOutcome,
        calls: RefCell<Vec<(Option<String>, Choice)>>,
    }

    impl Tapper for FakeTapper {
        fn tap(&self, phone: Option<String>, choice: Choice) -> Result<TapOutcome, TapError> {
            self.calls.borrow_mut().push((phone, choice));
            Ok(self.outcome.clone())
        }
    }

    const TOKEN: &str = "ABCD-EFGH-JKMN-PQRS";

    fn server(outcome: TapOutcome) -> Server<FakeTapper> {
        let config = Config { port: 1, token: TOKEN.into(), ttl_seconds: 120 };
        Server::new(&config, FakeTapper { outcome, calls: RefCell::new(Vec::new()) })
    }

    fn request(raw: &str) -> Request {
        read_request(&mut Cursor::new(raw.as_bytes().to_vec())).unwrap()
    }

    fn post_tap(auth: &str, body: &str) -> Request {
        request(&format!(
            "POST /tap HTTP/1.1\r\nHost: pc\r\nAuthorization: {auth}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ))
    }

    fn json(resp: &Response) -> serde_json::Value {
        serde_json::from_str(&resp.body).unwrap()
    }

    const PEER: Option<IpAddr> = Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 9)));

    #[test]
    fn parses_content_length_body_and_query() {
        let req = request("POST /tap?x=1 HTTP/1.1\r\nContent-Length: 5\r\nX-A: b\r\n\r\nhello");
        assert_eq!((req.method.as_str(), req.path.as_str()), ("POST", "/tap"));
        assert_eq!(req.body, b"hello");
        assert_eq!(req.header("x-a"), Some("b"));
    }

    #[test]
    fn parses_chunked_body() {
        let req = request(
            "POST /tap HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n6;ext=1\r\npedia \r\n0\r\n\r\n",
        );
        assert_eq!(req.body, b"Wikipedia ");
    }

    #[test]
    fn rejects_oversized_and_truncated_requests() {
        let big = format!("POST /tap HTTP/1.1\r\nContent-Length: {}\r\n\r\n", MAX_BODY + 1);
        assert_eq!(read_request(&mut Cursor::new(big.into_bytes())).unwrap_err().0, 413);
        let short = "POST /tap HTTP/1.1\r\nContent-Length: 10\r\n\r\nabc";
        assert_eq!(read_request(&mut Cursor::new(short.as_bytes().to_vec())).unwrap_err().0, 400);
        let head = format!("GET / HTTP/1.1\r\nX: {}\r\n", "a".repeat(MAX_HEAD + 10));
        assert_eq!(read_request(&mut Cursor::new(head.into_bytes())).unwrap_err().0, 431);
    }

    #[test]
    fn token_is_required_and_normalized() {
        let mut s = server(TapOutcome::Nothing);
        let now = Instant::now();
        assert_eq!(s.respond(PEER, &post_tap("Bearer WRONG-TOKEN-0000-0000", "{}"), now).status, 401);
        assert_eq!(s.respond(PEER, &post_tap("Bearer abcdefghjkmnpqrs", "{}"), now).status, 200);
        let alt = request("GET /ping HTTP/1.1\r\nX-TapClip-Token: ABCD EFGH JKMN PQRS\r\n\r\n");
        assert_eq!(s.respond(PEER, &alt, now).status, 200);
    }

    #[test]
    fn repeated_bad_tokens_lock_the_peer_out() {
        let mut s = server(TapOutcome::Nothing);
        let now = Instant::now();
        for _ in 0..LOCKOUT_FAILURES {
            assert_eq!(s.respond(PEER, &post_tap("Bearer nope", "{}"), now).status, 401);
        }
        // Even the right token is refused while locked...
        assert_eq!(s.respond(PEER, &post_tap(&format!("Bearer {TOKEN}"), "{}"), now).status, 429);
        // ...until the window passes.
        let later = now + LOCKOUT_WINDOW + Duration::from_secs(1);
        assert_eq!(s.respond(PEER, &post_tap(&format!("Bearer {TOKEN}"), "{}"), later).status, 200);
    }

    #[test]
    fn tap_passes_text_and_choice_through() {
        let mut s = server(TapOutcome::Sent);
        let resp =
            s.respond(PEER, &post_tap(&format!("Bearer {TOKEN}"), r#"{"text":"hi","choice":"send"}"#), Instant::now());
        assert_eq!(resp.status, 200);
        assert_eq!(s.tapper.calls.borrow()[0], (Some("hi".into()), Choice::Send));
        let body = json(&resp);
        assert!(body["message"].as_str().unwrap().contains("2 分钟"));
        assert!(body.get("copy").is_none() && body.get("ask").is_none());
    }

    #[test]
    fn received_reply_carries_copy_and_ask_reply_carries_ask() {
        let mut s = server(TapOutcome::Received("line one\nline two".into()));
        let resp = s.respond(PEER, &post_tap(&format!("Bearer {TOKEN}"), ""), Instant::now());
        let body = json(&resp);
        assert_eq!(body["copy"], "line one\nline two");
        assert_eq!(body["message"], "⬇ 已从电脑取来：line one…");

        let mut s = server(TapOutcome::Ask);
        let body = json(&s.respond(PEER, &post_tap(&format!("Bearer {TOKEN}"), r#"{"text":"x"}"#), Instant::now()));
        assert_eq!(body["ask"], "1");
    }

    #[test]
    fn unknown_paths_and_bad_json_are_rejected() {
        let mut s = server(TapOutcome::Nothing);
        let now = Instant::now();
        assert_eq!(s.respond(PEER, &request("GET /admin HTTP/1.1\r\n\r\n"), now).status, 404);
        assert_eq!(s.respond(PEER, &post_tap(&format!("Bearer {TOKEN}"), "not json"), now).status, 400);
    }

    #[test]
    fn preview_is_one_short_line() {
        assert_eq!(preview("short"), "short");
        assert_eq!(preview("  padded  "), "padded");
        assert_eq!(preview(&"字".repeat(30)), format!("{}…", "字".repeat(24)));
    }
}
