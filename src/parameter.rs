use mime::Mime;
use serde::Serialize;

use std::{fs, io::Read, os::unix::fs::MetadataExt};

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

#[cfg(test)]
mod testing {
    use super::*;

    #[test]
    fn test_mime_parsing() {
        let test_type = "text/plain;charset=UTF-8";
        let parsed_mime = test_type.parse::<Mime>().unwrap();
        assert_eq!(parsed_mime.essence_str(), "text/plain");
        assert_eq!(parsed_mime.get_param("charset").unwrap(), "UTF-8");
        assert_eq!(parsed_mime, test_type)
    }
}
