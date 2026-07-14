use http::header::ToStrError;
use mime::FromStrError;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("curl error: {0}")]
    CurlError(#[from] curl::Error),
    #[error("curl form error: {0}")]
    CurlFormError(#[from] curl::FormError),
    #[error("invalid header name: {0}")]
    InvalidHeaderName(#[from] http::header::InvalidHeaderName),
    #[error("invalid header value: {0}")]
    InvalidHeaderValue(#[from] http::header::InvalidHeaderValue),
    #[error("URL parse error: {0}")]
    UrlParserError(#[from] url::ParseError),
    #[error("file error: {0}")]
    FileError(#[from] std::io::Error),
    #[error("header value is not valid UTF-8: {0}")]
    ToStringError(#[from] ToStrError),
    #[error("header parse error: {0}")]
    HeaderParseError(String),
    #[error("MIME parse error: {0}")]
    MimeParseError(#[from] FromStrError),
    #[error("UTF-8 error: {0}")]
    Utf8Error(#[from] std::str::Utf8Error),
}

pub type Result<T> = std::result::Result<T, Error>;
