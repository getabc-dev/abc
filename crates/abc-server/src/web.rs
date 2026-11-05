//! The web app (`web/`), built by `cargo xtask build`, embedded in the `abc` binary and served on
//! the sidecar listener beside the API.
//!
//! | Request | Answer |
//! | --- | --- |
//! | `/api` or anything under `/api/` | the API's own, never the app |
//! | a file under `/assets/` | the file, cached for a year: its name holds its content hash |
//! | `/` or another file of the app | the file, revalidated on every use (`no-cache`) |
//! | a missing file under `/assets/` | `404 not_found` |
//! | any other path | `index.html`, whose router shows the page or its own not-found page |
//!
//! Only `GET` and `HEAD` reach the app; other methods get `405 method_not_allowed`. Every file is
//! sent with `X-Content-Type-Options: nosniff`, and `index.html` with a content security policy
//! that lets the page load only the app's own files and call only its own server.
//!
//! The app holds no data: everything a person sees comes from the API, which checks identity on
//! every request. So the files are served to any caller the sidecar lets through.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_LENGTH, CONTENT_SECURITY_POLICY, CONTENT_TYPE, REFERRER_POLICY,
    X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::Response;
use thiserror::Error;

/// The page the router answers every path of its own with.
const INDEX: &str = "index.html";

/// The directory of files whose names hold their content hash, which Vite writes.
const ASSETS: &str = "assets/";

/// How long a browser may keep a hashed file without asking again: a year, and never revalidated,
/// since a new build names its files anew.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// The page's content security policy: its own scripts, styles, images and fonts, requests to its
/// own server only, and no framing, plugins, base URL changes or form posts elsewhere.
const POLICY: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' \
    data:; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; \
    form-action 'self'; frame-ancestors 'none'";

/// The content type of each extension the app may hold.
const CONTENT_TYPES: [(&str, &str); 12] = [
    ("html", "text/html; charset=utf-8"),
    ("js", "text/javascript; charset=utf-8"),
    ("mjs", "text/javascript; charset=utf-8"),
    ("css", "text/css; charset=utf-8"),
    ("json", "application/json"),
    ("map", "application/json"),
    ("webmanifest", "application/manifest+json"),
    ("svg", "image/svg+xml"),
    ("png", "image/png"),
    ("ico", "image/x-icon"),
    ("woff2", "font/woff2"),
    ("txt", "text/plain; charset=utf-8"),
];

/// A file of the app, ready to send.
#[derive(Debug)]
struct File {
    bytes: &'static [u8],
    content_type: &'static str,
    /// Whether its name holds its content hash, so it can be kept for a year.
    hashed: bool,
}

/// The built web app: its files by path, checked when it is made.
#[derive(Debug, Clone)]
pub struct WebApp(Arc<Files>);

#[derive(Debug)]
struct Files {
    /// `index.html`.
    index: File,
    /// The other files, by path.
    others: BTreeMap<&'static str, File>,
}

/// Why a set of files is not a web app the server can serve.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum WebAppError {
    /// The build has no `index.html`.
    #[error("the web app has no index.html; build abc with `cargo xtask build`")]
    NoIndex,
    /// A path that is not plain, relative and URL-safe.
    #[error(
        "the web app's file {path:?} is not a relative path of letters, digits and `._-` in \
         `/`-separated parts"
    )]
    Path {
        /// The path.
        path: String,
    },
    /// A file whose content type the server does not know.
    #[error(
        "the web app's file {path} has no known content type; add its extension to \
         CONTENT_TYPES in crates/abc-server/src/web.rs"
    )]
    ContentType {
        /// The path.
        path: String,
    },
    /// A file under `assets/` whose name holds no content hash, so caching it for a year would
    /// keep it stale.
    #[error(
        "the web app's file {path} is under assets/ but its name holds no content hash \
         (`<name>-<hash>.<extension>`); let Vite name it"
    )]
    Unhashed {
        /// The path.
        path: String,
    },
    /// The same path twice.
    #[error("the web app holds {path} twice")]
    Duplicate {
        /// The path.
        path: String,
    },
}

/// What the app answers a request that no API route matched.
#[derive(Debug)]
pub(crate) enum Answer {
    /// A file, or `index.html` for a path of the router's.
    File(Response),
    /// Nothing here: an API path, or a missing hashed file.
    NotFound,
    /// A method other than `GET` and `HEAD`.
    MethodNotAllowed,
}

impl WebApp {
    /// The app made of `files`, each a path relative to the build's directory with its contents.
    ///
    /// # Errors
    ///
    /// [`WebAppError`] when there is no `index.html`, or a file's path, extension or name is not
    /// one the server can serve as the module says.
    pub fn new(files: &[(&'static str, &'static [u8])]) -> Result<Self, WebAppError> {
        let mut others = BTreeMap::new();
        let mut index = None;
        for &(path, bytes) in files {
            let error = |kind: fn(String) -> WebAppError| kind(path.to_owned());
            if !plain(path) {
                return Err(error(|named| WebAppError::Path { path: named }));
            }
            let content_type = path
                .rsplit_once('.')
                .and_then(|(_, extension)| {
                    CONTENT_TYPES.iter().find(|(known, _)| *known == extension)
                })
                .map(|(_, content_type)| *content_type)
                .ok_or_else(|| error(|named| WebAppError::ContentType { path: named }))?;
            let hashed = path.starts_with(ASSETS);
            if hashed && !named_by_hash(path) {
                return Err(error(|named| WebAppError::Unhashed { path: named }));
            }
            let file = File {
                bytes,
                content_type,
                hashed,
            };
            let duplicate = if path == INDEX {
                index.replace(file).is_some()
            } else {
                others.insert(path, file).is_some()
            };
            if duplicate {
                return Err(error(|named| WebAppError::Duplicate { path: named }));
            }
        }
        let index = index.ok_or(WebAppError::NoIndex)?;
        Ok(Self(Arc::new(Files { index, others })))
    }

    /// The answer to `method` on `path`, a request no API route matched.
    pub(crate) fn answer(&self, method: &Method, path: &str) -> Answer {
        if path == "/api" || path.starts_with("/api/") {
            return Answer::NotFound;
        }
        if method != Method::GET && method != Method::HEAD {
            return Answer::MethodNotAllowed;
        }
        let relative = path.strip_prefix('/').unwrap_or(path);
        let file = match self.0.others.get(relative) {
            Some(file) => file,
            None if relative.starts_with(ASSETS) => return Answer::NotFound,
            // The page itself, and every path of its router.
            None => &self.0.index,
        };
        Answer::File(send(file, method == Method::HEAD))
    }
}

/// `file` as a response: its body, or none for `HEAD`, with its caching and security headers.
fn send(file: &File, head: bool) -> Response {
    let length = file.bytes.len();
    let mut response = Response::new(if head {
        Body::empty()
    } else {
        Body::from(file.bytes)
    });
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(file.content_type));
    headers.insert(CONTENT_LENGTH, HeaderValue::from(length));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    if file.hashed {
        headers.insert(CACHE_CONTROL, HeaderValue::from_static(IMMUTABLE));
    } else {
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(POLICY));
    }
    response
}

/// Whether `path` is relative, `/`-separated, and each part non-empty, not `.` or `..`, and made
/// of letters, digits and `._-`: a path a URL names as it is.
fn plain(path: &str) -> bool {
    path.split('/').all(|part| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    })
}

/// Whether the file name of `path` is `<name>-<hash>.<extension>`, as Vite names a hashed file
/// (`hashCharacters: "hex"` in web/vite.config.ts): at least 8 lowercase hex digits after the
/// name's last `-`.
fn named_by_hash(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let stem = name.split('.').next().unwrap_or(name);
    stem.rsplit_once('-').is_some_and(|(before, hash)| {
        !before.is_empty()
            && hash.len() >= 8
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

#[cfg(test)]
mod tests {
    use axum::http::header::{
        CACHE_CONTROL, CONTENT_LENGTH, CONTENT_SECURITY_POLICY, CONTENT_TYPE,
        X_CONTENT_TYPE_OPTIONS,
    };
    use axum::http::{Method, StatusCode};
    use axum::response::Response;

    use super::{Answer, IMMUTABLE, POLICY, WebApp, WebAppError};

    const INDEX: &[u8] = b"<!doctype html><title>abc</title>";
    const SCRIPT: &[u8] = b"console.log(1)";

    fn app() -> WebApp {
        WebApp::new(&[
            ("index.html", INDEX),
            ("assets/index-b8678073.js", SCRIPT),
            ("assets/index-244cdaf5.css", b"p{}"),
            ("robots.txt", b"User-agent: *"),
        ])
        .unwrap()
    }

    fn file(answer: Answer) -> Response {
        match answer {
            Answer::File(response) => response,
            other @ (Answer::NotFound | Answer::MethodNotAllowed) => {
                panic!("expected a file, got {other:?}")
            }
        }
    }

    fn header<'a>(response: &'a Response, name: &axum::http::HeaderName) -> Option<&'a str> {
        response
            .headers()
            .get(name)
            .map(|value| value.to_str().unwrap())
    }

    #[test]
    fn serves_index_html_for_the_root_and_the_routers_paths() {
        let app = app();
        for path in [
            "/",
            "/index.html",
            "/machines",
            "/workspaces/1/settings",
            "/a.b",
        ] {
            let response = file(app.answer(&Method::GET, path));
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(
                header(&response, &CONTENT_TYPE),
                Some("text/html; charset=utf-8"),
                "{path}"
            );
            assert_eq!(
                header(&response, &CACHE_CONTROL),
                Some("no-cache"),
                "{path}"
            );
            assert_eq!(
                header(&response, &CONTENT_SECURITY_POLICY),
                Some(POLICY),
                "{path}"
            );
            assert_eq!(
                header(&response, &CONTENT_LENGTH),
                Some(INDEX.len().to_string().as_str()),
                "{path}"
            );
        }
    }

    #[test]
    fn hashed_assets_are_kept_for_a_year() {
        let response = file(app().answer(&Method::GET, "/assets/index-b8678073.js"));
        assert_eq!(
            header(&response, &CONTENT_TYPE),
            Some("text/javascript; charset=utf-8")
        );
        assert_eq!(header(&response, &CACHE_CONTROL), Some(IMMUTABLE));
        assert_eq!(header(&response, &X_CONTENT_TYPE_OPTIONS), Some("nosniff"));
        assert_eq!(
            header(&response, &CONTENT_SECURITY_POLICY),
            None,
            "the policy is the page's"
        );
        let css = file(app().answer(&Method::GET, "/assets/index-244cdaf5.css"));
        assert_eq!(header(&css, &CONTENT_TYPE), Some("text/css; charset=utf-8"));
    }

    #[test]
    fn other_files_are_revalidated() {
        let response = file(app().answer(&Method::GET, "/robots.txt"));
        assert_eq!(
            header(&response, &CONTENT_TYPE),
            Some("text/plain; charset=utf-8")
        );
        assert_eq!(header(&response, &CACHE_CONTROL), Some("no-cache"));
    }

    #[test]
    fn head_has_the_headers_and_no_body() {
        let response = file(app().answer(&Method::HEAD, "/assets/index-b8678073.js"));
        assert_eq!(
            header(&response, &CONTENT_LENGTH),
            Some(SCRIPT.len().to_string().as_str())
        );
        assert_eq!(
            axum::body::HttpBody::size_hint(response.body()).exact(),
            Some(0)
        );
    }

    #[test]
    fn never_answers_for_the_api_or_a_missing_asset() {
        let app = app();
        for path in [
            "/api",
            "/api/",
            "/api/session",
            "/api/nothing/here",
            "/assets/gone-0123abcd.js",
            "/assets/",
        ] {
            assert!(
                matches!(app.answer(&Method::GET, path), Answer::NotFound),
                "{path}"
            );
        }
        assert!(
            matches!(app.answer(&Method::POST, "/api/x"), Answer::NotFound),
            "the API's paths are the API's, whatever the method"
        );
        assert!(
            matches!(app.answer(&Method::GET, "/apiary"), Answer::File(_)),
            "only the /api segment is the API's"
        );
    }

    #[test]
    fn only_get_and_head_reach_the_app() {
        let app = app();
        for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
            assert!(
                matches!(app.answer(&method, "/"), Answer::MethodNotAllowed),
                "{method}"
            );
        }
    }

    #[test]
    fn refuses_files_it_cannot_serve_as_built() {
        let refused = |files: &[(&'static str, &'static [u8])]| WebApp::new(files).unwrap_err();
        let path = |path: &str| path.to_owned();
        assert_eq!(refused(&[]), WebAppError::NoIndex);
        assert_eq!(
            refused(&[("assets/a-12345678.js", b"")]),
            WebAppError::NoIndex
        );
        for bad in [
            "/index.html",
            "../index.html",
            "a//b.js",
            "./a.js",
            "a b.js",
            "a%20b.js",
            "",
        ] {
            assert_eq!(
                refused(&[("index.html", b""), (bad, b"")]),
                WebAppError::Path { path: path(bad) },
                "{bad:?}"
            );
        }
        for unknown in ["README", "a.exe", "a.htm"] {
            assert_eq!(
                refused(&[("index.html", b""), (unknown, b"")]),
                WebAppError::ContentType {
                    path: path(unknown)
                },
                "{unknown}"
            );
        }
        for unhashed in [
            "assets/index.js",
            "assets/index-0123abc.js",
            "assets/-0123abcd.js",
            "assets/index-0123ABCD.js",
            "assets/index-0123abcg.js",
        ] {
            assert_eq!(
                refused(&[("index.html", b""), (unhashed, b"")]),
                WebAppError::Unhashed {
                    path: path(unhashed)
                },
                "{unhashed}"
            );
        }
        WebApp::new(&[("index.html", b""), ("assets/chunk-a-0123abcd.js", b"")])
            .expect("a name may hold `-` itself");
        assert_eq!(
            refused(&[("index.html", b""), ("index.html", b"")]),
            WebAppError::Duplicate {
                path: path("index.html")
            }
        );
    }

    #[test]
    fn errors_say_what_to_do() {
        assert!(
            WebAppError::NoIndex
                .to_string()
                .contains("`cargo xtask build`")
        );
        assert!(
            WebAppError::ContentType {
                path: "a.exe".to_owned()
            }
            .to_string()
            .contains("add its extension to CONTENT_TYPES")
        );
    }
}
