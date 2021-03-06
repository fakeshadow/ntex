//! Request logging middleware
use std::collections::HashSet;
use std::convert::TryFrom;
use std::env;
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::future::{ok, Ready};
use regex::Regex;
use time::OffsetDateTime;

use crate::http::body::{BodySize, MessageBody, ResponseBody};
use crate::http::header::HeaderName;
use crate::service::{Service, Transform};
use crate::web::dev::{WebRequest, WebResponse};
use crate::web::HttpResponse;

/// `Middleware` for logging request and response info to the terminal.
///
/// `Logger` middleware uses standard log crate to log information. You should
/// enable logger for `actix_web` package to see access log.
/// ([`env_logger`](https://docs.rs/env_logger/*/env_logger/) or similar)
///
/// ## Usage
///
/// Create `Logger` middleware with the specified `format`.
/// Default `Logger` could be created with `default` method, it uses the
/// default format:
///
/// ```ignore
///  %a "%r" %s %b "%{Referer}i" "%{User-Agent}i" %T
/// ```
/// ```rust
/// use ntex::web::App;
/// use ntex::web::middleware::Logger;
///
/// fn main() {
///     std::env::set_var("RUST_LOG", "actix_web=info");
///     env_logger::init();
///
///     let app = App::new()
///         .wrap(Logger::default())
///         .wrap(Logger::new("%a %{User-Agent}i"));
/// }
/// ```
///
/// ## Format
///
/// `%%`  The percent sign
///
/// `%a`  Remote IP-address (IP-address of proxy if using reverse proxy)
///
/// `%t`  Time when the request was started to process (in rfc3339 format)
///
/// `%r`  First line of request
///
/// `%s`  Response status code
///
/// `%b`  Size of response in bytes, including HTTP headers
///
/// `%T` Time taken to serve the request, in seconds with floating fraction in
/// .06f format
///
/// `%D`  Time taken to serve the request, in milliseconds
///
/// `%U`  Request URL
///
/// `%{FOO}i`  request.headers['FOO']
///
/// `%{FOO}o`  response.headers['FOO']
///
/// `%{FOO}e`  os.environ['FOO']
///
pub struct Logger<Err> {
    inner: Rc<Inner>,
    _t: PhantomData<Err>,
}

struct Inner {
    format: Format,
    exclude: HashSet<String>,
}

impl<Err> Logger<Err> {
    /// Create `Logger` middleware with the specified `format`.
    pub fn new(format: &str) -> Logger<Err> {
        Logger {
            inner: Rc::new(Inner {
                format: Format::new(format),
                exclude: HashSet::new(),
            }),
            _t: PhantomData,
        }
    }

    /// Ignore and do not log access info for specified path.
    pub fn exclude<T: Into<String>>(mut self, path: T) -> Self {
        Rc::get_mut(&mut self.inner)
            .unwrap()
            .exclude
            .insert(path.into());
        self
    }
}

impl<Err> Default for Logger<Err> {
    /// Create `Logger` middleware with format:
    ///
    /// ```ignore
    /// %a "%r" %s %b "%{Referer}i" "%{User-Agent}i" %T
    /// ```
    fn default() -> Self {
        Logger {
            inner: Rc::new(Inner {
                format: Format::default(),
                exclude: HashSet::new(),
            }),
            _t: PhantomData,
        }
    }
}

impl<S, B, Err> Transform<S> for Logger<Err>
where
    S: Service<Request = WebRequest<Err>, Response = WebResponse<B>>,
    B: MessageBody,
{
    type Request = WebRequest<Err>;
    type Response = WebResponse<StreamLog<B>>;
    type Error = S::Error;
    type InitError = ();
    type Transform = LoggerMiddleware<S, Err>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(LoggerMiddleware {
            service,
            inner: self.inner.clone(),
            _t: PhantomData,
        })
    }
}

/// Logger middleware
pub struct LoggerMiddleware<S, Err> {
    inner: Rc<Inner>,
    service: S,
    _t: PhantomData<Err>,
}

impl<S, B, E> Service for LoggerMiddleware<S, E>
where
    S: Service<Request = WebRequest<E>, Response = WebResponse<B>>,
    B: MessageBody,
{
    type Request = WebRequest<E>;
    type Response = WebResponse<StreamLog<B>>;
    type Error = S::Error;
    type Future = LoggerResponse<S, B, E>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        self.service.poll_shutdown(cx, is_error)
    }

    #[inline]
    fn call(&self, req: WebRequest<E>) -> Self::Future {
        if self.inner.exclude.contains(req.path()) {
            LoggerResponse {
                fut: self.service.call(req),
                format: None,
                time: OffsetDateTime::now(),
                _t: PhantomData,
            }
        } else {
            let now = OffsetDateTime::now();
            let mut format = self.inner.format.clone();

            for unit in &mut format.0 {
                unit.render_request(now, &req);
            }
            LoggerResponse {
                fut: self.service.call(req),
                format: Some(format),
                time: now,
                _t: PhantomData,
            }
        }
    }
}

#[doc(hidden)]
#[pin_project::pin_project]
pub struct LoggerResponse<S, B, E>
where
    B: MessageBody,
    S: Service,
{
    #[pin]
    fut: S::Future,
    time: OffsetDateTime,
    format: Option<Format>,
    _t: PhantomData<(B, E)>,
}

impl<S, B, E> Future for LoggerResponse<S, B, E>
where
    B: MessageBody,
    S: Service<Request = WebRequest<E>, Response = WebResponse<B>>,
{
    type Output = Result<WebResponse<StreamLog<B>>, S::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        let res = match futures::ready!(this.fut.poll(cx)) {
            Ok(res) => res,
            Err(e) => return Poll::Ready(Err(e)),
        };

        if let Some(ref mut format) = this.format {
            for unit in &mut format.0 {
                unit.render_response(res.response());
            }
        }

        let time = *this.time;
        let format = this.format.take();

        Poll::Ready(Ok(res.map_body(move |_, body| {
            ResponseBody::Body(StreamLog {
                body,
                time,
                format,
                size: 0,
            })
        })))
    }
}

pub struct StreamLog<B> {
    body: ResponseBody<B>,
    format: Option<Format>,
    size: usize,
    time: OffsetDateTime,
}

impl<B> Drop for StreamLog<B> {
    fn drop(&mut self) {
        if let Some(ref format) = self.format {
            let render = |fmt: &mut Formatter<'_>| {
                for unit in &format.0 {
                    unit.render(fmt, self.size, self.time)?;
                }
                Ok(())
            };
            log::info!("{}", FormatDisplay(&render));
        }
    }
}

impl<B: MessageBody> MessageBody for StreamLog<B> {
    fn size(&self) -> BodySize {
        self.body.size()
    }

    fn poll_next_chunk(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Box<dyn Error>>>> {
        match self.body.poll_next_chunk(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                self.size += chunk.len();
                Poll::Ready(Some(Ok(chunk)))
            }
            val => val,
        }
    }
}

/// A formatting style for the `Logger`, consisting of multiple
/// `FormatText`s concatenated into one line.
#[derive(Clone)]
#[doc(hidden)]
struct Format(Vec<FormatText>);

impl Default for Format {
    /// Return the default formatting style for the `Logger`:
    fn default() -> Format {
        Format::new(r#"%a "%r" %s %b "%{Referer}i" "%{User-Agent}i" %T"#)
    }
}

impl Format {
    /// Create a `Format` from a format string.
    ///
    /// Returns `None` if the format string syntax is incorrect.
    fn new(s: &str) -> Format {
        log::trace!("Access log format: {}", s);
        let fmt = Regex::new(r"%(\{([A-Za-z0-9\-_]+)\}([ioe])|[atPrUsbTD]?)").unwrap();

        let mut idx = 0;
        let mut results = Vec::new();
        for cap in fmt.captures_iter(s) {
            let m = cap.get(0).unwrap();
            let pos = m.start();
            if idx != pos {
                results.push(FormatText::Str(s[idx..pos].to_owned()));
            }
            idx = m.end();

            if let Some(key) = cap.get(2) {
                results.push(match cap.get(3).unwrap().as_str() {
                    "i" => FormatText::RequestHeader(
                        HeaderName::try_from(key.as_str()).unwrap(),
                    ),
                    "o" => FormatText::ResponseHeader(
                        HeaderName::try_from(key.as_str()).unwrap(),
                    ),
                    "e" => FormatText::EnvironHeader(key.as_str().to_owned()),
                    _ => unreachable!(),
                })
            } else {
                let m = cap.get(1).unwrap();
                results.push(match m.as_str() {
                    "%" => FormatText::Percent,
                    "a" => FormatText::RemoteAddr,
                    "t" => FormatText::RequestTime,
                    "r" => FormatText::RequestLine,
                    "s" => FormatText::ResponseStatus,
                    "b" => FormatText::ResponseSize,
                    "U" => FormatText::UrlPath,
                    "T" => FormatText::Time,
                    "D" => FormatText::TimeMillis,
                    _ => FormatText::Str(m.as_str().to_owned()),
                });
            }
        }
        if idx != s.len() {
            results.push(FormatText::Str(s[idx..].to_owned()));
        }

        Format(results)
    }
}

/// A string of text to be logged. This is either one of the data
/// fields supported by the `Logger`, or a custom `String`.
#[doc(hidden)]
#[derive(Debug, Clone)]
enum FormatText {
    Str(String),
    Percent,
    RequestLine,
    RequestTime,
    ResponseStatus,
    ResponseSize,
    Time,
    TimeMillis,
    RemoteAddr,
    UrlPath,
    RequestHeader(HeaderName),
    ResponseHeader(HeaderName),
    EnvironHeader(String),
}

impl FormatText {
    fn render(
        &self,
        fmt: &mut Formatter<'_>,
        size: usize,
        entry_time: OffsetDateTime,
    ) -> Result<(), fmt::Error> {
        match *self {
            FormatText::Str(ref string) => fmt.write_str(string),
            FormatText::Percent => "%".fmt(fmt),
            FormatText::ResponseSize => size.fmt(fmt),
            FormatText::Time => {
                let rt = OffsetDateTime::now() - entry_time;
                let rt = rt.as_seconds_f64();
                fmt.write_fmt(format_args!("{:.6}", rt))
            }
            FormatText::TimeMillis => {
                let rt = OffsetDateTime::now() - entry_time;
                let rt = (rt.whole_nanoseconds() as f64) / 1_000_000.0;
                fmt.write_fmt(format_args!("{:.6}", rt))
            }
            FormatText::EnvironHeader(ref name) => {
                if let Ok(val) = env::var(name) {
                    fmt.write_fmt(format_args!("{}", val))
                } else {
                    "-".fmt(fmt)
                }
            }
            _ => Ok(()),
        }
    }

    fn render_response<B>(&mut self, res: &HttpResponse<B>) {
        match *self {
            FormatText::ResponseStatus => {
                *self = FormatText::Str(format!("{}", res.status().as_u16()))
            }
            FormatText::ResponseHeader(ref name) => {
                let s = if let Some(val) = res.headers().get(name) {
                    if let Ok(s) = val.to_str() {
                        s
                    } else {
                        "-"
                    }
                } else {
                    "-"
                };
                *self = FormatText::Str(s.to_string())
            }
            _ => (),
        }
    }

    fn render_request<E>(&mut self, now: OffsetDateTime, req: &WebRequest<E>) {
        match *self {
            FormatText::RequestLine => {
                *self = if req.query_string().is_empty() {
                    FormatText::Str(format!(
                        "{} {} {:?}",
                        req.method(),
                        req.path(),
                        req.version()
                    ))
                } else {
                    FormatText::Str(format!(
                        "{} {}?{} {:?}",
                        req.method(),
                        req.path(),
                        req.query_string(),
                        req.version()
                    ))
                };
            }
            FormatText::UrlPath => *self = FormatText::Str(req.path().to_string()),
            FormatText::RequestTime => {
                *self = FormatText::Str(now.format("%Y-%m-%dT%H:%M:%S"))
            }
            FormatText::RequestHeader(ref name) => {
                let s = if let Some(val) = req.headers().get(name) {
                    if let Ok(s) = val.to_str() {
                        s
                    } else {
                        "-"
                    }
                } else {
                    "-"
                };
                *self = FormatText::Str(s.to_string());
            }
            FormatText::RemoteAddr => {
                let s = if let Some(remote) = req.connection_info().remote() {
                    FormatText::Str(remote.to_string())
                } else {
                    FormatText::Str("-".to_string())
                };
                *self = s;
            }
            _ => (),
        }
    }
}

pub(crate) struct FormatDisplay<'a>(
    &'a dyn Fn(&mut Formatter<'_>) -> Result<(), fmt::Error>,
);

impl<'a> fmt::Display for FormatDisplay<'a> {
    fn fmt(&self, fmt: &mut Formatter<'_>) -> Result<(), fmt::Error> {
        (self.0)(fmt)
    }
}

#[cfg(test)]
mod tests {
    use futures::future::ok;

    use super::*;
    use crate::http::{header, StatusCode};
    use crate::service::{IntoService, Service, Transform};
    use crate::web::test::TestRequest;
    use crate::web::{DefaultError, Error};

    #[ntex_rt::test]
    async fn test_logger() {
        let srv = |req: WebRequest<DefaultError>| {
            ok::<_, Error>(
                req.into_response(
                    HttpResponse::build(StatusCode::OK)
                        .header("X-Test", "ttt")
                        .finish(),
                ),
            )
        };
        let logger = Logger::new("%% %{User-Agent}i %{X-Test}o %{HOME}e %D test");

        let srv = Transform::new_transform(&logger, srv.into_service())
            .await
            .unwrap();

        let req = TestRequest::with_header(
            header::USER_AGENT,
            header::HeaderValue::from_static("ACTIX-WEB"),
        )
        .to_srv_request();
        let _res = srv.call(req).await;
    }

    #[ntex_rt::test]
    async fn test_url_path() {
        let mut format = Format::new("%T %U");
        let req = TestRequest::with_header(
            header::USER_AGENT,
            header::HeaderValue::from_static("ACTIX-WEB"),
        )
        .uri("/test/route/yeah")
        .to_srv_request();

        let now = OffsetDateTime::now();
        for unit in &mut format.0 {
            unit.render_request(now, &req);
        }

        let resp = HttpResponse::build(StatusCode::OK).force_close().finish();
        for unit in &mut format.0 {
            unit.render_response(&resp);
        }

        let render = |fmt: &mut Formatter<'_>| {
            for unit in &format.0 {
                unit.render(fmt, 1024, now)?;
            }
            Ok(())
        };
        let s = format!("{}", FormatDisplay(&render));
        assert!(s.contains("/test/route/yeah"));
    }

    #[ntex_rt::test]
    async fn test_default_format() {
        let mut format = Format::default();

        let req = TestRequest::with_header(
            header::USER_AGENT,
            header::HeaderValue::from_static("ACTIX-WEB"),
        )
        .to_srv_request();

        let now = OffsetDateTime::now();
        for unit in &mut format.0 {
            unit.render_request(now, &req);
        }

        let resp = HttpResponse::build(StatusCode::OK).force_close().finish();
        for unit in &mut format.0 {
            unit.render_response(&resp);
        }

        let entry_time = OffsetDateTime::now();
        let render = |fmt: &mut Formatter<'_>| {
            for unit in &format.0 {
                unit.render(fmt, 1024, entry_time)?;
            }
            Ok(())
        };
        let s = format!("{}", FormatDisplay(&render));
        assert!(s.contains("GET / HTTP/1.1"));
        assert!(s.contains("200 1024"));
        assert!(s.contains("ACTIX-WEB"));
    }

    #[ntex_rt::test]
    async fn test_request_time_format() {
        let mut format = Format::new("%t");
        let req = TestRequest::default().to_srv_request();

        let now = OffsetDateTime::now();
        for unit in &mut format.0 {
            unit.render_request(now, &req);
        }

        let resp = HttpResponse::build(StatusCode::OK).force_close().finish();
        for unit in &mut format.0 {
            unit.render_response(&resp);
        }

        let render = |fmt: &mut Formatter<'_>| {
            for unit in &format.0 {
                unit.render(fmt, 1024, now)?;
            }
            Ok(())
        };
        let s = format!("{}", FormatDisplay(&render));
        assert!(s.contains(&format!("{}", now.format("%Y-%m-%dT%H:%M:%S"))));
    }
}
