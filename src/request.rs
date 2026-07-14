use bytes::Bytes;
use http::header::{CONTENT_TYPE, HeaderMap};
use serde::Serialize;
use url::Url;
use urlencoding::encode;

use crate::error::Result;
use crate::parameter::{Parameter, ParameterType};

pub(crate) enum RequestBody {
    Raw {
        data: Bytes,
        content_type: Option<String>,
    },
    FormUrlEncoded(String),
    Multipart(Vec<MultipartPart>),
}

pub(crate) struct MultipartPart {
    pub(crate) name: String,
    pub(crate) data: Bytes,
    pub(crate) mime_type: Option<String>,
}

pub(crate) fn generate_base_url(url_str: &str) -> Result<(Url, Vec<Parameter>)> {
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

/// called when only a single simple body parameter or a single complex parameter is passed to the client (after transforming simple parameters into query parameters for GET calls)
/// If a simple parameter is provided, its name and value (if set) have to be url encoded
///
/// This will also set the corresponding content headers, if none was set
pub(crate) fn construct_singular_body(headers: &HeaderMap, parameter: Parameter) -> Result<RequestBody> {
    match parameter {
        Parameter::SimpleParameter { name, value, .. } => {
            let text = if value.is_empty() {
                encode(&name).into_owned()
            } else {
                format!("{}={}", encode(&name), encode(&value))
            };
            let content_type = if headers.contains_key(CONTENT_TYPE.as_str()) {
                None
            } else {
                Some(mime::APPLICATION_WWW_FORM_URLENCODED.to_string())
            };
            Ok(RequestBody::Raw {
                data: text.into_bytes().into(),
                content_type,
            })
        }
        Parameter::ComplexParameter {
            mime_type,
            content,
            ..
        } => {
            let content_type = if headers.contains_key(CONTENT_TYPE.as_str()) {
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

pub(crate) fn construct_multipart(parameters: Vec<Parameter>) -> Result<RequestBody> {
    let mut parts = Vec::new();
    for parameter in parameters {
        match parameter {
            Parameter::SimpleParameter { name, value, .. } => {
                // names/values are stored raw; multipart field parts are not percent-encoded
                parts.push(MultipartPart {
                    name,
                    data: value.into_bytes().into(),
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

pub(crate) fn construct_form_url_encoded(parameters: Vec<Parameter>) -> Result<RequestBody> {
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

#[derive(Serialize, Debug, Clone, PartialEq)]
pub enum Method {
    GET,
    PUT,
    POST,
    HEAD,
    DELETE,
    PATCH,
}
