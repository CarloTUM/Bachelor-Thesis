use derive_more::From;
use http::header::ToStrError;
use mime::FromStrError;

#[derive(From, Debug)]
pub enum Error {
    CurlError(curl::Error),
    CurlFormError(curl::FormError),
    InvalidHeaderName(http::header::InvalidHeaderName),
    InvalidHeaderValue(http::header::InvalidHeaderValue),
    UrlParserError(url::ParseError),
    FileError(std::io::Error),
    ToStringError(ToStrError),
    HeaderParseError(String),
    MimeParseError(FromStrError),
    Utf8Error(std::str::Utf8Error),
}

pub type Result<T> = std::result::Result<T, Error>;
