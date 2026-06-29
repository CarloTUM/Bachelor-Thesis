use bytes::{Buf, Bytes};
use curl::easy::{Easy, Form, List};
use derive_more::From;
use multipart::server::{FieldHeaders, ReadEntry};
use http::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, ToStrError};
use url::Url;
use serde::Serialize;
use tempfile::tempfile;

use std::{
    collections::HashMap, fmt::Debug, fs, io::{Read, Seek, Write}, os::unix::fs::MetadataExt, str::FromStr, sync::MutexGuard, time::Duration
};

pub use mime::*;

use urlencoding::encode;

#[derive(Debug, Clone, Serialize)]
pub enum ParameterType {
    Query,
    Body,
}

// Simple kv parameters, can be in body or url
// If a request is a get request -> All parameters into the query
// The name and value do not have to be escaped yet -> Part of generate_url
// Will always be UTF-8 encoded
#[derive(Debug)]
pub enum Parameter {
    SimpleParameter {
        name: String,
        value: String,
        param_type: ParameterType,
    },

    // Since File is not cloneable, we do not merge simple and complex parameters into an enum
    // For sending/receiving files
    ComplexParameter {
        name: String,
        //  If no charset is specified, the default is ASCII (US-ASCII) unless overridden by the user agent's settings (https://developer.mozilla.org/en-US/docs/Web/HTTP/Basics_of_HTTP/MIME_types)
        mime_type: Mime,
        content_handle: fs::File,
    },
}

#[derive(Serialize)]
pub enum ParameterDTO {
    SimpleParameterDTO {
        name: String,
        value: String,
        param_type: ParameterType,
    },

    // Since File is not cloneable, we do not merge simple and complex parameters into an enum
    // For sending/receiving files
    ComplexParameterDTO {
        name: String,
        //  If no charset is specified, the default is ASCII (US-ASCII) unless overridden by the user agent's settings (https://developer.mozilla.org/en-US/docs/Web/HTTP/Basics_of_HTTP/MIME_types)
        mime_type: String,
        value: Vec<u8>,
    },
}

impl Into<ParameterDTO> for Parameter {
    fn into(self) -> ParameterDTO {
        match self {
            Parameter::SimpleParameter {
                name,
                value,
                param_type,
            } => ParameterDTO::SimpleParameterDTO { name, value, param_type },
            Parameter::ComplexParameter {
                name,
                mime_type,
                mut content_handle,
            } => {
                let mut content = Vec::with_capacity(content_handle.metadata().map(|data| data.size()).unwrap_or(0).try_into().unwrap());
                content_handle.read_to_end(&mut content).expect("This should not fail");
                ParameterDTO::ComplexParameterDTO { name, mime_type: mime_type.essence_str().to_owned(), value: content }},
        }
    }
}

enum Headers {
    HeaderMap(HeaderMap),
    PartHeaders(FieldHeaders),
}

pub struct RawResponse {
    pub headers: HeaderMap,
    pub body: bytes::Bytes,
    pub status_code: u16,
}

pub struct ParsedResponse {
    pub headers: HashMap<String, String>,
    pub content: Vec<Parameter>,
    pub status_code: u16,
    pub raw: Bytes,
}

/// Abstraction on top of libcurl
pub struct Client {
    method: Method,
    base_url: Url,
    pub headers: HeaderMap,
    parameters: Vec<Parameter>,
    form_url_encoded: bool,
}

enum RequestBody {
    Raw {
        data: Vec<u8>,
        content_type: Option<String>,
    },
    FormUrlEncoded(String),
    Multipart(Vec<MultipartPart>),
}

struct MultipartPart {
    name: String,
    data: Vec<u8>,
    mime_type: Option<String>,
}

impl  Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("method", &self.method)
            .field("headers", &self.headers)
            .field("parameters", &self.parameters)
            .field("form_url_encoded", &self.form_url_encoded)
            .finish()
    }
}

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

type Result<T> = std::result::Result<T, Error>;

/// Executes HTTP requests:
///
///  - SimpleParameters for query parameters and application/x-www-form-urlencoded
///  - ComplexParameters for files (content-type of header/part-header is mime-type)
///  - If multiple parameters are provided, then a multipart (including complex params) or form url encoded (only simple params) request is send
///  - If the query string contains query parameters they are parsed into SimpleParameters (with ParameterType Query)
///  - SimpleParameter name and value are URL encoded
impl Client {
    pub fn new(url: &str, method: Method) -> Result<Client> {
        let (base_url, parameters) = generate_base_url(url)?;
        let mut client = Client {
            method,
            base_url,
            headers: HeaderMap::new(),
            // All simple parameters are URL encoded -> If added through add_parameters
            parameters: Vec::new(),
            form_url_encoded: true,
        };
        // Add clients via add to url encode them
        client.add_parameters(parameters);
        Ok(client)
    }

    /// Will add the parameter to the client parameters for the request
    pub fn add_parameter(&mut self, mut parameter: Parameter) {
        parameter = match parameter {
            Parameter::SimpleParameter {
                name,
                value,
                param_type,
            } => match param_type {
                ParameterType::Query => {
                    Parameter::SimpleParameter {
                        name: name.to_owned(),
                        value: value.to_owned(),
                        param_type,
                    }
                }
                ParameterType::Body => {
                    Parameter::SimpleParameter {
                        name: name.to_owned(),
                        value: value.to_owned(),
                        param_type,
                    }
                }
            },
            Parameter::ComplexParameter {
                name,
                mime_type,
                content_handle,
            } => {
                // If we add a complex parameter, we no longer can send the request as form url encoded
                self.form_url_encoded = false;
                Parameter::ComplexParameter {
                    name,
                    mime_type,
                    content_handle,
                }
            }
        };
        self.parameters.push(parameter);
    }

    /// Will add a complex (file-backed) parameter to the client parameters for the request
    /// The provided bytes are written to a temporary file used as the content handle
    pub fn add_complex_parameter(
        &mut self,
        name: &str,
        mime_type: Mime,
        data: &[u8],
    ) -> Result<()> {
        let mut file = tempfile()?;
        file.write_all(data)?;
        file.rewind()?;

        self.add_parameter(Parameter::ComplexParameter {
            name: name.to_owned(),
            mime_type,
            content_handle: file,
        });
        Ok(())
    }

    pub fn add_parameters(&mut self, parameters: Vec<Parameter>) {
        parameters.into_iter().for_each(|parameter| {
            self.add_parameter(parameter);
        });
    }

    pub fn set_request_headers(&mut self, request_headers: HeaderMap) {
        self.headers = request_headers;
    }

    /// Inserts the header and replace the previous value. Currently not supporting multi valued headers
    /// If header parameters are desired, provide them as part of the value (delimited by the ;)
    ///
    /// Only visible ASCII characters (32-127) are permitted. Use
    /// `from_bytes` to create a `HeaderValue` that includes opaque octets
    /// (128-255).
    pub fn add_request_header(&mut self, name: &str, value: &str) -> Result<()> {
        self.headers.remove(name);
        self.headers
            .insert(HeaderName::from_str(name)?, HeaderValue::from_str(value)?);
        Ok(())
    }

    /// Inserts the header and replace the previous value. Currently not supporting multi valued headers
    /// If header parameters are desired, provide them as part of the value (delimited by the ;)
    ///
    /// Only visible ASCII characters (32-127) are permitted. Use
    /// `from_bytes` to create a `HeaderValue` that includes opaque octets
    /// (128-255).
    pub fn add_request_headers(&mut self, headers: HashMap<String, String>) -> Result<()> {
        for (name, value) in headers.into_iter() {
            self.add_request_header(&name, &value)?;
        }
        Ok(())
    }

    /// Generates the complete request URL including the query parameters.
    /// Query parameters are constructed from the SimpleParameters with ParameterType Query
    fn generate_url(&self) -> Url {
        let mut query_params = Vec::new();
        self.parameters
            .iter()
            .for_each(|parameter| match parameter {
                Parameter::SimpleParameter {
                    name,
                    value,
                    param_type,
                } => {
                    let is_query_param = matches!(param_type, ParameterType::Query);
                    if is_query_param {
                        if value.len() == 0 {
                            query_params.push(encode(name).into_owned());
                        } else {
                            query_params.push(format!("{}={}", encode(name), encode(value)));
                        };
                    }
                }
                _ => (),
            });
        let query_string = query_params.join("&");
        let url = if query_string.is_empty() {
            // if we have no query parameters, just send the base url
            self.base_url.as_str().to_owned()
        } else {
            format!("{}?{}", self.base_url.as_str(), query_string)
        };
        Url::parse(&url).expect("Cannot happen")
    }

    /// Given the parameters/method the body and relevant headers are adjusted
    /// After this method, the parameters field is left as an empty vector
    fn generate_body(&mut self) -> Result<RequestBody> {
        let parameters: Vec<Parameter> = std::mem::replace(&mut self.parameters, Vec::new());
        let mut body_parameters: Vec<Parameter> = parameters
            .into_iter()
            .filter(|parameter| match parameter {
                Parameter::SimpleParameter { param_type, .. } => {
                    matches!(param_type, ParameterType::Body)
                }
                Parameter::ComplexParameter { .. } => true,
            })
            .collect();
        if body_parameters.len() == 1 {
            self.construct_singular_body(body_parameters.pop().expect("Cannot fail"))
        } else {
            // For multipart we set a multipart content type => Remove custom content type
            self.headers.remove(CONTENT_TYPE.as_str());
            if self.form_url_encoded {
                construct_form_url_encoded(body_parameters)
            } else {
                construct_multipart(body_parameters)
            }
        }
    }

    /// Will execute the request and return the RawResponse
    /// Requires the target to send headers that only contain visible ascii
    pub fn execute_raw(self) -> Result<RawResponse> {
        let mut easy = Easy::new();
        self.execute_on(&mut easy)
    }

    fn execute_on(mut self, easy: &mut Easy) -> Result<RawResponse> {
        let url = self.generate_url();
        easy.url(url.as_str())?;
        easy.timeout(Duration::from_secs(20))?;
        easy.follow_location(true)?;
        easy.max_redirections(10)?;
        easy.custom_request(match self.method {
            Method::GET => "GET",
            Method::POST => "POST",
            Method::PUT => "PUT",
            Method::HEAD => "HEAD",
            Method::DELETE => "DELETE",
            Method::PATCH => "PATCH",
        })?;
        if matches!(self.method, Method::HEAD) {
            easy.nobody(true)?;
        }

        let body = self.generate_body()?;
        match body {
            RequestBody::Raw { data, content_type } => {
                if let Some(ct) = content_type {
                    self.headers.insert(CONTENT_TYPE, HeaderValue::from_str(&ct)?);
                }
                if !data.is_empty() {
                    easy.post_fields_copy(&data)?;
                }
            }
            RequestBody::FormUrlEncoded(encoded) => {
                self.headers.insert(
                    CONTENT_TYPE,
                    HeaderValue::from_static("application/x-www-form-urlencoded"),
                );
                easy.post_fields_copy(encoded.as_bytes())?;
            }
            RequestBody::Multipart(parts) => {
                let mut form = Form::new();
                for part in &parts {
                    let mut p = form.part(&part.name);
                    p.contents(&part.data);
                    if let Some(mime) = &part.mime_type {
                        p.content_type(mime);
                    }
                    p.add()?;
                }
                easy.httppost(form)?;
            }
        }

        let mut header_list = List::new();
        for (name, value) in self.headers.iter() {
            header_list.append(&format!("{}: {}", name.as_str(), value.to_str()?))?;
        }
        easy.http_headers(header_list)?;

        let mut response_body: Vec<u8> = Vec::new();
        let mut response_headers = HeaderMap::new();
        let mut header_err: Option<Error> = None;
        {
            let mut transfer = easy.transfer();
            transfer.write_function(|data| {
                response_body.extend_from_slice(data);
                Ok(data.len())
            })?;
            transfer.header_function(|line| {
                if header_err.is_none() {
                    if let Err(e) = process_header_line(line, &mut response_headers) {
                        header_err = Some(e);
                    }
                }
                true
            })?;
            transfer.perform()?;
        }
        if let Some(e) = header_err {
            return Err(e);
        }

        Ok(RawResponse {
            headers: response_headers,
            status_code: easy.response_code()? as u16,
            body: bytes::Bytes::from(response_body),
        })
    }

    /// Executes the request and consumes the client as the headers and parameters are consumed by the request
    pub fn execute(self) -> Result<ParsedResponse> {
        let raw = self.execute_raw()?;

        raw.parse_response()
    }

    /// called when only a single simple body parameter or a single complex parameter is passed to the client (after transforming simple parameters into query parameters for GET calls)
    /// If a simple parameter is provided, its name and value (if set) have to be url encoded
    ///
    /// This will also set the corresponding content headers, if none was set
    fn construct_singular_body(
        &mut self,
        parameter: Parameter,
    ) -> Result<RequestBody> {
        match parameter {
            Parameter::SimpleParameter { name, value, .. } => {
                let text = if value.len() == 0 {
                    encode(&name).into_owned()
                } else {
                    format!("{}={}", encode(&name), encode(&value))
                };
                let content_type = if self.headers.contains_key(CONTENT_TYPE.as_str()) {
                    None
                } else {
                    Some(mime::APPLICATION_WWW_FORM_URLENCODED.to_string())
                };
                Ok(RequestBody::Raw {
                    data: text.into_bytes(),
                    content_type,
                })
            }
            Parameter::ComplexParameter {
                mime_type,
                mut content_handle,
                ..
            } => {
                // We read out the content handle, otherwise we could stream in the file read (better) but then it would use transfer-encoding chunked -> currently not supported
                let mut content = Vec::new();
                content_handle.rewind()?;
                content_handle.read_to_end(&mut content)?;
                let content_type = if self.headers.contains_key(CONTENT_TYPE.as_str()) {
                    None
                } else {
                    Some(mime_type.to_string())
                };
                Ok(RequestBody::Raw {
                    data: content,
                    content_type,
                })
            }
        }
    }
}

/// Keeps one curl Easy handle around and reuses it, so we don't redo the
/// TCP/TLS handshake on every call to the same host. Each request reset()s
/// the handle to drop the old options but keep the live connection.
pub struct Agent {
    easy: Easy,
}

impl Agent {
    pub fn new() -> Agent {
        Agent { easy: Easy::new() }
    }

    pub fn execute_raw(&mut self, client: Client) -> Result<RawResponse> {
        self.easy.reset();
        client.execute_on(&mut self.easy)
    }

    pub fn execute(&mut self, client: Client) -> Result<ParsedResponse> {
        self.execute_raw(client)?.parse_response()
    }

    pub fn local_port(&mut self) -> Result<u16> {
        Ok(self.easy.local_port()?)
    }
}

impl Default for Agent {
    fn default() -> Self {
        Agent::new()
    }
}

fn generate_base_url(url_str: &str) -> Result<(Url, Vec<Parameter>)> {
    // Remove trailing whitespace
    let url_str = url_str.trim_end();
    let split = url_str.split_once("?");
    match split {
        Some((base_url_str, query_str)) => {
            let base_url = Url::parse(base_url_str)?;
            let parameters = parse_query_string(query_str);
            Ok((base_url, parameters))
        }
        // URL does not contain any query parameters
        None => Ok((Url::parse(url_str)?, Vec::new())),
    }
}

/// Parses the query string (the section after <base-url>?)
/// Will not URL encode it
fn parse_query_string(query: &str) -> Vec<Parameter> {
    query
        .split("&")
        .map(|parameter| {
            match parameter.split_once("=") {
                // Key value pair
                Some((name, value)) => Parameter::SimpleParameter {
                    name: name.to_owned(),
                    value: value.to_owned(),
                    param_type: ParameterType::Query,
                },
                // query parameter without a value
                None => Parameter::SimpleParameter {
                    name: parameter.to_owned(),
                    value: String::new(),
                    param_type: ParameterType::Query,
                },
            }
        })
        .collect()
}

fn construct_multipart(parameters: Vec<Parameter>) -> Result<RequestBody> {
    let mut parts = Vec::new();
    for parameter in parameters {
        match parameter {
            Parameter::SimpleParameter { name, value, .. } => {
                // names/values are stored raw; multipart field parts are not percent-encoded
                parts.push(MultipartPart {
                    name,
                    data: value.into_bytes(),
                    mime_type: None,
                });
            }
            Parameter::ComplexParameter {
                name,
                mime_type,
                mut content_handle,
            } => {
                let mut content = Vec::new();
                // We read out the content handle, otherwise we could stream in the file read (better) but then it would use transfer-encoding chunked -> currently not supported
                content_handle.rewind()?;
                content_handle.read_to_end(&mut content)?;
                parts.push(MultipartPart {
                    name,
                    data: content,
                    mime_type: Some(mime_type.to_string()),
                });
            }
        }
    }
    Ok(RequestBody::Multipart(parts))
}

fn construct_form_url_encoded(parameters: Vec<Parameter>) -> Result<RequestBody> {
    let mut params = Vec::new();
    for parameter in parameters {
        match parameter {
            Parameter::SimpleParameter { name, value, .. } => {
                if value.len() == 0 {
                    params.push(encode(&name).into_owned());
                } else {
                    params.push(format!("{}={}", encode(&name), encode(&value)));
                };
            }
            Parameter::ComplexParameter { .. } => {
                panic!("not all parameters are simple! Cannot construct form url encoded body")
            }
        }
    }
    Ok(RequestBody::FormUrlEncoded(params.join("&")))
}

/// Processes a single header line from libcurl's header callback.
/// Clears the HeaderMap when a new "HTTP/" status line arrives so headers from
/// earlier phases (e.g. "100 Continue") do not bleed into the final response.
/// Returns Err for non-UTF8 lines or header names/values that are not valid
/// per http::header — stricter than reqwest, which surfaced this lazily at
/// to_str(). Unobservable under the visible-ASCII precondition.
fn process_header_line(line: &[u8], headers: &mut HeaderMap) -> Result<()> {
    let s = std::str::from_utf8(line)
        .map_err(|_| Error::HeaderParseError("non-utf8 header line".to_owned()))?;
    let trimmed = s.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        return Ok(());
    }
    if trimmed.starts_with("HTTP/") {
        headers.clear();
        return Ok(());
    }
    let Some((name, value)) = trimmed.split_once(':') else {
        return Ok(());
    };
    let n = HeaderName::from_str(name.trim())?;
    let v = HeaderValue::from_str(value.trim())?;
    headers.append(n, v);
    Ok(())
}

/// Will be lossy if a header has multiple values.
pub fn header_map_to_hash_map(headers: &HeaderMap) -> Result<HashMap<String, String>> {
    let mut header_map = HashMap::with_capacity(headers.keys_len());
    for (name, value) in headers.into_iter() {
        header_map.insert(name.as_str().to_owned(), value.to_str()?.to_owned());
    }
    Ok(header_map)
}

impl RawResponse {
    fn parse_response(self) -> Result<ParsedResponse> {
        Ok(ParsedResponse {
            headers: header_map_to_hash_map(&self.headers)?,
            content: parse_part(Headers::HeaderMap(self.headers), &self.body)?,
            status_code: self.status_code,
            raw: self.body,
        })
    }
}

/// Parses the response with their corresponding headers and body
/// For non-multipart responses this will terminate after one method invocation
/// For multipart responses this is called recursive for each part.
fn parse_part(headers: Headers, body: &[u8]) -> Result<Vec<Parameter>> {
    let (name, content_type) = get_name_and_content_type(&headers)?;
    // We use essence_str to remove any attached parameters for this comparison
    if content_type.type_() == mime::MULTIPART {
        let boundary = content_type
            .get_param(BOUNDARY)
            .ok_or(Error::HeaderParseError(
                "Content type multipart misses boundary parameter".to_owned(),
            ))?
            .to_string();
        parse_multipart(body, &boundary)
    } else if content_type.essence_str() == mime::APPLICATION_WWW_FORM_URLENCODED {
        parse_form_urlencoded(body)
    } else {
        parse_flat_data(&content_type, body, &name)
    }
}

fn get_name_and_content_type(headers: &Headers) -> Result<(String, Mime)> {
    let name = match headers {
        Headers::HeaderMap(headers) => match headers.get(HeaderName::from_str("content-id")?) {
            Some(content_id) => content_id.to_str()?.trim().replace("\"", ""),
            None => "result".to_owned(),
        },
        Headers::PartHeaders(headers) => {
            // Get arc as ref
            let mut name = &*headers.name;
            if name.len() == 0 {
                name = "result";
            }
            name.to_owned()
        }
    };
    let content_type = match headers {
        Headers::HeaderMap(headers) => match headers.get(CONTENT_TYPE) {
            Some(content_type) => content_type.to_str()?.trim().parse::<mime::Mime>()?,
            None => APPLICATION_OCTET_STREAM,
        }
        .to_owned(),
        Headers::PartHeaders(headers) => headers
            .content_type
            .clone()
            .unwrap_or(APPLICATION_OCTET_STREAM),
    };
    Ok((name, content_type))
}

/// Parses content into a single complex parameter
fn parse_flat_data(content_type: &Mime, body: &[u8], name: &str) -> Result<Vec<Parameter>> {
    let mut content = tempfile::tempfile()?;
    content.write(&body)?;
    content.rewind()?;
    Ok(vec![Parameter::ComplexParameter {
        name: name.to_owned(),
        mime_type: content_type.clone(),
        content_handle: content,
    }])
}

/// Parses content into list of simple parameters (& separated sequence)
fn parse_form_urlencoded(body: &[u8]) -> Result<Vec<Parameter>> {
    let mut parameters = Vec::new();
    form_urlencoded::parse(&body).for_each(|pair| {
        // UTF 8 per standard: https://url.spec.whatwg.org/#urlencoded-parsing
        parameters.push(Parameter::SimpleParameter {
            name: (*pair.0).to_owned(),
            value: (*pair.1).to_owned(),
            param_type: ParameterType::Body,
        });
    });
    Ok(parameters)
}

fn parse_multipart(body: &[u8], boundary: &str) -> Result<Vec<Parameter>> {
    let mut parameters: Vec<Parameter> = Vec::new();
    let mut multipart = multipart::server::Multipart::with_body(body.reader(), boundary);
    loop {
        let part = multipart.read_entry_mut();
        match part {
            multipart::server::ReadEntryResult::Entry(mut entry) => {
                let mut body: Vec<u8> = Vec::new();
                entry.data.read_to_end(&mut body)?;
                parameters.extend(parse_part(Headers::PartHeaders(entry.headers), &body)?)
            }
            multipart::server::ReadEntryResult::End(_) => return Ok(parameters),
            multipart::server::ReadEntryResult::Error(_, error) => {
                eprintln!("Ran into error during reading of multipart: {}", error);
                return Err(Error::from(error));
            }
        };
    }
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub enum Method {
    GET,
    PUT,
    POST,
    HEAD,
    DELETE,
    PATCH,
}

#[cfg(test)]
mod testing {
    use super::*;
    use std::io::{Read, Seek};

    #[test]
    fn test_mime_parsing() {
        let test_type = "text/plain;charset=UTF-8";
        let parsed_mime = test_type.parse::<Mime>().unwrap();
        assert_eq!(parsed_mime.essence_str(), "text/plain");
        assert_eq!(parsed_mime.get_param("charset").unwrap(), "UTF-8");
        assert_eq!(parsed_mime, test_type)
    }

    #[test]
    fn test_header_callback_clears_on_new_phase() {
        let mut h = HeaderMap::new();
        process_header_line(b"HTTP/1.1 100 Continue\r\n", &mut h).unwrap();
        process_header_line(b"Link: </preload>; rel=preload\r\n", &mut h).unwrap();
        process_header_line(b"\r\n", &mut h).unwrap();
        process_header_line(b"HTTP/1.1 200 OK\r\n", &mut h).unwrap();
        process_header_line(b"Content-Type: text/plain\r\n", &mut h).unwrap();
        process_header_line(b"X-Trace: abc\r\n", &mut h).unwrap();
        assert!(h.get("link").is_none(), "100-phase header must not leak");
        assert_eq!(h.get("content-type").unwrap(), "text/plain");
        assert_eq!(h.get("x-trace").unwrap(), "abc");
    }

    #[test]
    fn test_header_callback_rejects_non_ascii() {
        let mut h = HeaderMap::new();
        assert!(process_header_line(b"X-Bad: \xff\xfe\r\n", &mut h).is_err());
    }

    mod test_creation {
        use super::*;

        #[test]
        fn test_url_generation() -> Result<()> {
            let test_url = "https://www.testing.com?test=value&tes. == aba\"";
            let client = Client::new(test_url, Method::GET)?;
            println!("{:?}", client.parameters);
            assert_eq!(
                // checked with urlencoder.org
                "https://www.testing.com/?test=value&tes.%20=%3D%20aba%22",
                client.generate_url().to_string()
            );
            Ok(())
        }

        #[test]
        fn test_url_generation_with_params() -> Result<()> {
            let test_url = "https://www.testing.com/?test=value&onlyname";
            let mut client = Client::new(test_url, Method::GET)?;
            let parameters = vec![
                Parameter::SimpleParameter {
                    name: "a".to_owned(),
                    value: "a1".to_owned(),
                    param_type: ParameterType::Body,
                },
                Parameter::SimpleParameter {
                    name: "b".to_owned(),
                    value: "b1".to_owned(),
                    param_type: ParameterType::Query,
                },
                Parameter::SimpleParameter {
                    name: "c".to_owned(),
                    value: "".to_owned(),
                    param_type: ParameterType::Query,
                },
            ];
            client.add_parameters(parameters);

            assert_eq!(
                "https://www.testing.com/?test=value&onlyname&b=b1&c",
                client.generate_url().to_string()
            );
            Ok(())
        }

        #[test]
        fn test_building_singular_simple_body() -> Result<()> {
            let test_url = "http://localhost:5678";
            let mut client = Client::new(test_url, Method::POST)?;
            client.add_parameter(Parameter::SimpleParameter {
                name: "simple_param_ test".to_owned(),
                value: "simple_value".to_owned(),
                param_type: ParameterType::Body,
            });
            match client.generate_body()? {
                RequestBody::Raw { data, content_type } => {
                    assert_eq!(
                        content_type.as_deref(),
                        Some(APPLICATION_WWW_FORM_URLENCODED.as_ref())
                    );
                    assert_eq!(data, b"simple_param_%20test=simple_value");
                }
                _ => panic!("expected RequestBody::Raw"),
            }
            Ok(())
        }

        #[test]
        fn test_building_singular_complex_text_body() -> Result<()> {
            let test_url = "http://localhost:5678";
            let mut client = Client::new(test_url, Method::POST)?;
            let file = "./test_files/text/file_example.xml";
            let file = fs::File::open(file)?;

            client.add_parameter(Parameter::ComplexParameter {
                name: "test_file".to_owned(),
                mime_type: mime::TEXT_XML,
                content_handle: file,
            });
            match client.generate_body()? {
                RequestBody::Raw { content_type, .. } => {
                    assert_eq!(content_type.as_deref(), Some("text/xml"));
                }
                _ => panic!("expected RequestBody::Raw"),
            }
            Ok(())
        }

        #[test]
        fn test_building_singular_complex_binary_body() -> Result<()> {
            let test_url = "http://localhost:5678";
            let mut client = Client::new(test_url, Method::POST)?;
            let test_file = "./test_files/binary/16x16.jpg";
            let file = fs::File::open(test_file)?;

            client.add_parameter(Parameter::ComplexParameter {
                name: "test_file".to_owned(),
                mime_type: mime::IMAGE_JPEG,
                content_handle: file,
            });
            match client.generate_body()? {
                RequestBody::Raw { content_type, .. } => {
                    assert_eq!(content_type.as_deref(), Some("image/jpeg"));
                }
                _ => panic!("expected RequestBody::Raw"),
            }
            Ok(())
        }

        #[test]
        fn test_building_text_multipart() -> Result<()> {
            let test_url = "http://localhost:5678";
            let mut client = Client::new(test_url, Method::POST)?;
            client.add_parameter(Parameter::SimpleParameter {
                name: "simple_param_0test".to_owned(),
                value: "simple_value0".to_owned(),
                param_type: ParameterType::Body,
            });
            client.add_parameter(Parameter::SimpleParameter {
                name: "simple_param_1test".to_owned(),
                value: "simple_value1".to_owned(),
                param_type: ParameterType::Body,
            });
            client.add_parameter(Parameter::SimpleParameter {
                name: "simple_param_2test".to_owned(),
                value: "simple_value2".to_owned(),
                param_type: ParameterType::Body,
            });
            match client.generate_body()? {
                RequestBody::FormUrlEncoded(encoded) => {
                    assert_eq!(
                        encoded,
                        "simple_param_0test=simple_value0&simple_param_1test=simple_value1&simple_param_2test=simple_value2"
                    );
                }
                _ => panic!("expected RequestBody::FormUrlEncoded"),
            }
            Ok(())
        }

        #[test]
        fn test_building_mixed_multipart() -> Result<()> {
            let test_url = "http://localhost:5678";
            let mut client = Client::new(test_url, Method::POST)?;

            let test_file = "./test_files/binary/16x16.jpg";
            let file = fs::File::open(test_file)?;
            client.add_parameter(Parameter::ComplexParameter {
                name: "test_jpg".to_owned(),
                mime_type: mime::IMAGE_JPEG,
                content_handle: file,
            });

            let test_file = "./test_files/text/file_example.xml";
            let file = fs::File::open(test_file)?;
            client.add_parameter(Parameter::ComplexParameter {
                name: "test_xml".to_owned(),
                mime_type: mime::TEXT_XML,
                content_handle: file,
            });

            client.add_parameter(Parameter::SimpleParameter {
                name: "test_simple".to_owned(),
                value: "test_value".to_owned(),
                param_type: ParameterType::Body,
            });

            match client.generate_body()? {
                RequestBody::Multipart(parts) => {
                    assert_eq!(parts.len(), 3);
                    assert_eq!(parts[0].name, "test_jpg");
                    assert_eq!(parts[0].mime_type.as_deref(), Some("image/jpeg"));
                    assert_eq!(parts[1].name, "test_xml");
                    assert_eq!(parts[1].mime_type.as_deref(), Some("text/xml"));
                    assert_eq!(parts[2].name, "test_simple");
                    assert_eq!(parts[2].mime_type, None);
                    assert_eq!(parts[2].data, b"test_value");
                }
                _ => panic!("expected RequestBody::Multipart"),
            }
            Ok(())
        }
    }

    mod test_parsing {
        use mime::TEXT_PLAIN_UTF_8;

        use super::*;

        #[test]
        fn test_simple_parameter_parsing() -> Result<()> {
            let headers =
                parse_headers_from_file("./test_files/http/headers/simple_singular_headers.txt")?;
            println!("Headers: {:?}", headers);
            let body = fs::read("./test_files/http/bodies/simple_singular_body.txt")?;
            let mut result = parse_part(Headers::HeaderMap(headers), &body)?;
            assert_eq!(result.len(), 1);
            let result = result.pop().unwrap();
            match result {
                Parameter::SimpleParameter {
                    name,
                    value,
                    param_type,
                } => {
                    assert_eq!(name, "simple_param_ test");
                    assert_eq!(value, "simple_value");
                    assert!(matches!(param_type, ParameterType::Body));
                }
                Parameter::ComplexParameter { .. } => panic!("Should be simple_parameter"),
            }
            Ok(())
        }

        #[test]
        fn test_complex_parameter_parsing_text_file() -> Result<()> {
            let headers = parse_headers_from_file(
                "./test_files/http/headers/text_file_singular_headers.txt",
            )?;
            println!("Headers: {:?}", headers);
            let body = fs::read("./test_files/http/bodies/text_file_singular_body.txt")?;
            let mut result = parse_part(Headers::HeaderMap(headers), &body)?;
            println!("{:?}", result);
            match result.pop().unwrap() {
                Parameter::SimpleParameter { .. } => panic!("Should not happen"),
                Parameter::ComplexParameter {
                    mut content_handle,
                    mime_type,
                    ..
                } => {
                    println!("{:?}", content_handle);
                    let mut buffer = Vec::new();
                    content_handle.read_to_end(&mut buffer)?;
                    assert_eq!(body, buffer);
                    assert_eq!(mime_type.get_param("charset").unwrap(), "utf-8");
                }
            };
            Ok(())
        }

        #[test]
        fn test_complex_parameter_parsing_binary_file() -> Result<()> {
            let headers =
                parse_headers_from_file("./test_files/http/headers/jpg_file_singular_headers.txt")?;
            let body = fs::read("./test_files/http/bodies/jpg_file_singular_body.txt")?;
            let mut result = parse_part(Headers::HeaderMap(headers), &body)?;
            match result.pop().unwrap() {
                Parameter::SimpleParameter { .. } => panic!("Should not happen"),
                Parameter::ComplexParameter {
                    name,
                    mime_type,
                    mut content_handle,
                } => {
                    // Test custom name via content-id header:
                    assert_eq!(name, "moon.jpg");
                    assert_eq!(mime_type, APPLICATION_OCTET_STREAM);
                    let mut buffer = Vec::new();
                    content_handle.read_to_end(&mut buffer)?;
                    assert_eq!(body, buffer);
                    // Using custom name:
                }
            };
            Ok(())
        }

        #[test]
        fn test_text_multipart_parsing() -> Result<()> {
            let headers =
                parse_headers_from_file("./test_files/http/headers/text_multipart_headers.txt")?;
            println!("Headers: {:?}", headers);
            let body = fs::read("./test_files/http/bodies/text_multipart_body.txt")?;
            let result = parse_part(Headers::HeaderMap(headers), &body)?;
            assert_eq!(result.len(), 3);
            for (index, parameter) in result.into_iter().enumerate() {
                println!("Checking parameter {}", index);
                match parameter {
                    Parameter::SimpleParameter { .. } => panic!("Should not happen"),
                    Parameter::ComplexParameter {
                        name,
                        mime_type,
                        mut content_handle,
                    } => {
                        assert_eq!(mime_type, TEXT_PLAIN_UTF_8);
                        assert_eq!(name, format!("simple_param_{}test", index));

                        let mut content = String::new();
                        content_handle.read_to_string(&mut content)?;
                        assert_eq!(content, format!("simple_value{}", index));
                    }
                }
            }

            Ok(())
        }

        #[test]
        fn test_mixed_multipart_parsing() -> Result<()> {
            let headers =
                parse_headers_from_file("./test_files/http/headers/mixed_multipart_headers.txt")?;
            println!("Headers: {:?}", headers);
            let body = fs::read("./test_files/http/bodies/mixed_multipart_body.txt")?;
            let mut result = parse_part(Headers::HeaderMap(headers), &body)?;
            assert_eq!(result.len(), 3);
            result.iter().for_each(|parameter| {
                assert!(matches!(parameter, Parameter::ComplexParameter { .. }))
            });
            let expected_names = ["test_jpg", "test_xml", "test_simple"];
            let expected_mime_types = ["image/jpeg", "text/xml", "text/plain; charset=utf-8"];

            let text_value: Vec<u8> = "test_value".bytes().collect();
            let xml_content: Vec<u8> =
                fs::read("./test_files/text/file_example.xml").expect("Failed reading xml");
            let image: Vec<u8> =
                fs::read("./test_files/binary/16x16.jpg").expect("Failed reading jpg");
            let expected_content = [image, xml_content, text_value];

            result
                .iter_mut()
                .enumerate()
                .for_each(|(index, parameter)| match parameter {
                    Parameter::SimpleParameter { .. } => panic!("Cannot happen."),
                    Parameter::ComplexParameter {
                        name,
                        mime_type,
                        content_handle,
                    } => {
                        assert_eq!(name, expected_names[index]);
                        assert_eq!(mime_type.to_string(), expected_mime_types[index]);

                        let mut content = Vec::new();
                        content_handle
                            .read_to_end(&mut content)
                            .expect("Error reading parameter content");
                        assert_eq!(content, expected_content[index]);
                    }
                });

            println!("{:?}", result);
            Ok(())
        }

        #[test]
        fn test_construct_form_url_encoded_body() {
            let mut client = Client::new("http://test.org", crate::Method::GET).unwrap();
            client.add_parameters(vec![
                Parameter::SimpleParameter {
                    name: "a".to_owned(),
                    value: "b".to_owned(),
                    param_type: ParameterType::Body,
                },
                Parameter::SimpleParameter {
                    name: "c".to_owned(),
                    value: "d".to_owned(),
                    param_type: ParameterType::Body,
                },
            ]);
            match client.generate_body().unwrap() {
                RequestBody::FormUrlEncoded(encoded) => {
                    assert_eq!(encoded, "a=b&c=d");
                }
                _ => panic!("expected RequestBody::FormUrlEncoded"),
            }
        }

        #[test]
        fn test_construct_form_url_encoded_body_not_if_complex() {
            let mut client = Client::new("http://test.org", crate::Method::GET).unwrap();
            client.add_parameters(vec![
                Parameter::SimpleParameter {
                    name: "a".to_owned(),
                    value: "b".to_owned(),
                    param_type: ParameterType::Body,
                },
                Parameter::SimpleParameter {
                    name: "c".to_owned(),
                    value: "d".to_owned(),
                    param_type: ParameterType::Body,
                },
                Parameter::ComplexParameter {
                    name: "a".to_owned(),
                    mime_type: TEXT_PLAIN,
                    content_handle: tempfile::tempfile().unwrap(),
                },
            ]);
            match client.generate_body().unwrap() {
                RequestBody::Multipart(parts) => {
                    assert_eq!(parts.len(), 3);
                }
                _ => panic!("expected RequestBody::Multipart"),
            }
        }

        fn parse_headers_from_file(path: &str) -> Result<HeaderMap> {
            let header_string = fs::read_to_string(path)?.replace("\r", "");

            let mut headers = HeaderMap::new();
            header_string
                .split("\n")
                .map(|header| -> Result<(HeaderName, HeaderValue)> {
                    let (name, value) = header
                        .split_once(":")
                        .ok_or(Error::HeaderParseError("Does not contain :".to_owned()))?;
                    Ok((
                        HeaderName::from_str(name.trim())?,
                        HeaderValue::from_str(value.trim())?,
                    ))
                })
                .filter_map(|result| match result {
                    Ok((header_name, header_value)) => Some((header_name, header_value)),
                    Err(_) => None,
                })
                .for_each(|entry| {
                    headers.insert(entry.0, entry.1);
                });
            Ok(headers)
        }
    }

    /// Copies bytes from the 16x16.jpg from the multipart directly into a new file to check for correctness
    fn _copy_result() -> Result<()> {
        let test_file = "./scripts/output-multipart-mixed.txt";
        let mut file = fs::File::open(test_file)?;
        file.seek(std::io::SeekFrom::Start(0x190))?;
        let mut buffer: &mut [u8] = &mut [0; 0x1C19];
        file.read(&mut buffer)?;
        fs::write("./scripts/output.jpg", buffer)?;
        Ok(())
    }
}
