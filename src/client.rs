use bytes::Bytes;
use curl::easy::{Easy, Form, List};
use http::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use mime::Mime;
use url::Url;
use urlencoding::encode;

use std::{
    collections::HashMap, cell::RefCell, fmt::Debug, str::FromStr, time::Duration
};

use crate::error::{Error, Result};
use crate::parameter::{Parameter, ParameterType};
use crate::request::{
    Method, RequestBody, construct_form_url_encoded, construct_multipart,
    construct_singular_body, generate_base_url,
};
use crate::response::{ParsedResponse, RawResponse, process_header_line};

/// Abstraction on top of libcurl
pub struct Client {
    method: Method,
    base_url: Url,
    pub headers: HeaderMap,
    parameters: Vec<Parameter>,
    form_url_encoded: bool,
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
            content: Bytes::copy_from_slice(data),
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
            construct_singular_body(&self.headers, body_parameters.pop().expect("Cannot fail"))
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
            // reset() also keeps libcurl's cookie store, but the cookie engine
            // is never enabled here, so no cookie state survives between requests
            easy.reset();
            let result = self.execute_on(easy);
            // free the request's copied buffers right away instead of holding
            // them until the thread's next request; keeps the same caches
            easy.reset();
            result
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
                assert_eq!(data, b"simple_param_%20test=simple_value".as_slice());
            }
            _ => panic!("expected RequestBody::Raw"),
        }
        Ok(())
    }

    #[test]
    fn test_building_singular_complex_text_body() -> Result<()> {
        let test_url = "http://localhost:5678";
        let mut client = Client::new(test_url, Method::POST)?;
        let content: Bytes = fs::read("./test_files/text/file_example.xml")?.into();

        client.add_parameter(Parameter::ComplexParameter {
            name: "test_file".to_owned(),
            mime_type: mime::TEXT_XML,
            content: content.clone(),
        });
        match client.generate_body()? {
            RequestBody::Raw { data, content_type } => {
                assert_eq!(content_type.as_deref(), Some("text/xml"));
                assert_eq!(data, content);
            }
            _ => panic!("expected RequestBody::Raw"),
        }
        Ok(())
    }

    #[test]
    fn test_building_singular_complex_binary_body() -> Result<()> {
        let test_url = "http://localhost:5678";
        let mut client = Client::new(test_url, Method::POST)?;
        let content: Bytes = fs::read("./test_files/binary/16x16.jpg")?.into();

        client.add_parameter(Parameter::ComplexParameter {
            name: "test_file".to_owned(),
            mime_type: mime::IMAGE_JPEG,
            content: content.clone(),
        });
        match client.generate_body()? {
            RequestBody::Raw { data, content_type } => {
                assert_eq!(content_type.as_deref(), Some("image/jpeg"));
                assert_eq!(data, content);
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

        let jpg_content: Bytes = fs::read("./test_files/binary/16x16.jpg")?.into();
        client.add_parameter(Parameter::ComplexParameter {
            name: "test_jpg".to_owned(),
            mime_type: mime::IMAGE_JPEG,
            content: jpg_content.clone(),
        });

        let xml_content: Bytes = fs::read("./test_files/text/file_example.xml")?.into();
        client.add_parameter(Parameter::ComplexParameter {
            name: "test_xml".to_owned(),
            mime_type: mime::TEXT_XML,
            content: xml_content.clone(),
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
                assert_eq!(parts[0].data, jpg_content);
                assert_eq!(parts[1].name, "test_xml");
                assert_eq!(parts[1].mime_type.as_deref(), Some("text/xml"));
                assert_eq!(parts[1].data, xml_content);
                assert_eq!(parts[2].name, "test_simple");
                assert_eq!(parts[2].mime_type, None);
                assert_eq!(parts[2].data, b"test_value".as_slice());
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
                content: Bytes::new(),
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
