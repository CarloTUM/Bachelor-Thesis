use bytes::{Buf, Bytes};
use http::header::{CONTENT_TYPE, HeaderMap, HeaderName};
use mime::{APPLICATION_OCTET_STREAM, BOUNDARY, Mime};
use multipart::server::{FieldHeaders, ReadEntry};

use std::{
    collections::HashMap, io::Read, str::FromStr
};

use crate::error::{Error, Result};
use crate::parameter::{Parameter, ParameterType};

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

/// Will be lossy if a header has multiple values.
pub fn header_map_to_hash_map(headers: &HeaderMap) -> Result<HashMap<String, String>> {
    let mut header_map = HashMap::with_capacity(headers.keys_len());
    for (name, value) in headers.into_iter() {
        header_map.insert(name.as_str().to_owned(), value.to_str()?.to_owned());
    }
    Ok(header_map)
}

impl RawResponse {
    pub(crate) fn parse_response(self) -> Result<ParsedResponse> {
        Ok(ParsedResponse {
            headers: header_map_to_hash_map(&self.headers)?,
            // Bytes::clone is only a refcount bump; for flat responses the
            // parsed parameter then shares this buffer with `raw`
            content: parse_part(Headers::HeaderMap(self.headers), self.body.clone())?,
            status_code: self.status_code,
            raw: self.body,
        })
    }
}

/// Parses the response with their corresponding headers and body
/// For non-multipart responses this will terminate after one method invocation
/// For multipart responses this is called recursive for each part.
fn parse_part(headers: Headers, body: Bytes) -> Result<Vec<Parameter>> {
    let (name, content_type) = get_name_and_content_type(&headers)?;
    // We use essence_str to remove any attached parameters for this comparison
    if content_type.type_() == mime::MULTIPART {
        let boundary = content_type
            .get_param(BOUNDARY)
            .ok_or(Error::HeaderParseError(
                "Content type multipart misses boundary parameter".to_owned(),
            ))?
            .to_string();
        parse_multipart(&body, &boundary)
    } else if content_type.essence_str() == mime::APPLICATION_WWW_FORM_URLENCODED {
        parse_form_urlencoded(&body)
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
            if name.is_empty() {
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
fn parse_flat_data(content_type: &Mime, body: Bytes, name: &str) -> Result<Vec<Parameter>> {
    Ok(vec![Parameter::ComplexParameter {
        name: name.to_owned(),
        mime_type: content_type.clone(),
        content: body,
    }])
}

/// Parses content into list of simple parameters (& separated sequence)
fn parse_form_urlencoded(body: &[u8]) -> Result<Vec<Parameter>> {
    let mut parameters = Vec::new();
    form_urlencoded::parse(body).for_each(|pair| {
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
                // Bytes::from(Vec) takes ownership without copying
                parameters.extend(parse_part(Headers::PartHeaders(entry.headers), body.into())?)
            }
            multipart::server::ReadEntryResult::End(_) => return Ok(parameters),
            multipart::server::ReadEntryResult::Error(_, error) => {
                eprintln!("Ran into error during reading of multipart: {}", error);
                return Err(Error::from(error));
            }
        };
    }
}

#[cfg(test)]
mod test_parsing {
    use http::header::HeaderValue;
    use mime::TEXT_PLAIN_UTF_8;

    use super::*;

    use std::fs;

    #[test]
    fn test_simple_parameter_parsing() -> Result<()> {
        let headers =
            parse_headers_from_file("./test_files/http/headers/simple_singular_headers.txt")?;
        println!("Headers: {:?}", headers);
        let body = fs::read("./test_files/http/bodies/simple_singular_body.txt")?;
        let mut result = parse_part(Headers::HeaderMap(headers), body.into())?;
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
        let mut result = parse_part(Headers::HeaderMap(headers), body.clone().into())?;
        println!("{:?}", result);
        match result.pop().unwrap() {
            Parameter::SimpleParameter { .. } => panic!("Should not happen"),
            Parameter::ComplexParameter {
                content,
                mime_type,
                ..
            } => {
                assert_eq!(body, content);
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
        let mut result = parse_part(Headers::HeaderMap(headers), body.clone().into())?;
        match result.pop().unwrap() {
            Parameter::SimpleParameter { .. } => panic!("Should not happen"),
            Parameter::ComplexParameter {
                name,
                mime_type,
                content,
            } => {
                // Test custom name via content-id header:
                assert_eq!(name, "moon.jpg");
                assert_eq!(mime_type, APPLICATION_OCTET_STREAM);
                assert_eq!(body, content);
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
        let result = parse_part(Headers::HeaderMap(headers), body.into())?;
        assert_eq!(result.len(), 3);
        for (index, parameter) in result.into_iter().enumerate() {
            println!("Checking parameter {}", index);
            match parameter {
                Parameter::SimpleParameter { .. } => panic!("Should not happen"),
                Parameter::ComplexParameter {
                    name,
                    mime_type,
                    content,
                } => {
                    assert_eq!(mime_type, TEXT_PLAIN_UTF_8);
                    assert_eq!(name, format!("simple_param_{}test", index));

                    assert_eq!(content, format!("simple_value{}", index).into_bytes());
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
        let mut result = parse_part(Headers::HeaderMap(headers), body.into())?;
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
                    content,
                } => {
                    assert_eq!(name, expected_names[index]);
                    assert_eq!(mime_type.to_string(), expected_mime_types[index]);

                    assert_eq!(*content, expected_content[index]);
                }
            });

        println!("{:?}", result);
        Ok(())
    }

    /// Documents the behavior of the third-party mime parser that
    /// get_name_and_content_type relies on
    #[test]
    fn test_mime_parsing() {
        let test_type = "text/plain;charset=UTF-8";
        let parsed_mime = test_type.parse::<Mime>().unwrap();
        assert_eq!(parsed_mime.essence_str(), "text/plain");
        assert_eq!(parsed_mime.get_param("charset").unwrap(), "UTF-8");
        assert_eq!(parsed_mime, test_type)
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
