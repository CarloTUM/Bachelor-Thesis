use curl::easy::{Easy, Form, List};
use http::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use mime::Mime;
use serde::Serialize;
use url::Url;
use urlencoding::encode;

use std::{
    collections::HashMap, cell::RefCell, fmt::Debug, str::FromStr, time::Duration
};

use crate::error::{Error, Result};
use crate::parameter::{Parameter, ParameterType};
use crate::response::{ParsedResponse, RawResponse};

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

thread_local! {
    /// One libcurl handle per thread, reused across requests. `reset()` clears
    /// the previous request's options but keeps libcurl's live connection cache,
    /// so repeat calls to the same host skip the TCP/TLS handshake.
    static HANDLE: RefCell<Easy> = RefCell::new(Easy::new());
}

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
                content,
            } => {
                // If we add a complex parameter, we no longer can send the request as form url encoded
                self.form_url_encoded = false;
                Parameter::ComplexParameter {
                    name,
                    mime_type,
                    content,
                }
            }
        };
        self.parameters.push(parameter);
    }

    /// Will add a complex parameter to the client parameters for the request
    /// The provided bytes are stored in memory
    pub fn add_complex_parameter(
        &mut self,
        name: &str,
        mime_type: Mime,
        data: &[u8],
    ) -> Result<()> {
        self.add_parameter(Parameter::ComplexParameter {
            name: name.to_owned(),
            mime_type,
            content: data.to_vec(),
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
            .for_each(|parameter| if let Parameter::SimpleParameter {
                    name,
                    value,
                    param_type,
                } = parameter {
                let is_query_param = matches!(param_type, ParameterType::Query);
                if is_query_param {
                    if value.is_empty() {
                        query_params.push(encode(name).into_owned());
                    } else {
                        query_params.push(format!("{}={}", encode(name), encode(value)));
                    };
                }
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
        let parameters: Vec<Parameter> = std::mem::take(&mut self.parameters);
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
    ///
    /// Reuses a thread-local libcurl handle so connections to the same host stay
    /// alive across calls. The handle is reset() before each request to drop the
    /// previous request's options while keeping the live connection cache.
    pub fn execute_raw(self) -> Result<RawResponse> {
        HANDLE.with_borrow_mut(|easy| {
            easy.reset();
            self.execute_on(easy)
        })
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
                let text = if value.is_empty() {
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
                content,
                ..
            } => {
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
                content,
            } => {
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
                if value.is_empty() {
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
mod test_creation {
    use mime::APPLICATION_WWW_FORM_URLENCODED;

    use super::*;

    use std::fs;

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
        let content = fs::read("./test_files/text/file_example.xml")?;

        client.add_parameter(Parameter::ComplexParameter {
            name: "test_file".to_owned(),
            mime_type: mime::TEXT_XML,
            content,
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
        let content = fs::read("./test_files/binary/16x16.jpg")?;

        client.add_parameter(Parameter::ComplexParameter {
            name: "test_file".to_owned(),
            mime_type: mime::IMAGE_JPEG,
            content,
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

        let content = fs::read("./test_files/binary/16x16.jpg")?;
        client.add_parameter(Parameter::ComplexParameter {
            name: "test_jpg".to_owned(),
            mime_type: mime::IMAGE_JPEG,
            content,
        });

        let content = fs::read("./test_files/text/file_example.xml")?;
        client.add_parameter(Parameter::ComplexParameter {
            name: "test_xml".to_owned(),
            mime_type: mime::TEXT_XML,
            content,
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

#[cfg(test)]
mod test_parsing {
    use mime::TEXT_PLAIN;

    use super::*;

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
                content: Vec::new(),
            },
        ]);
        match client.generate_body().unwrap() {
            RequestBody::Multipart(parts) => {
                assert_eq!(parts.len(), 3);
            }
            _ => panic!("expected RequestBody::Multipart"),
        }
    }
}
