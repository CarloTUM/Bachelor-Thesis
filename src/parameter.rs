use bytes::Bytes;
use mime::Mime;
use serde::Serialize;

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

    // For sending/receiving files
    ComplexParameter {
        name: String,
        //  If no charset is specified, the default is ASCII (US-ASCII) unless overridden by the user agent's settings (https://developer.mozilla.org/en-US/docs/Web/HTTP/Basics_of_HTTP/MIME_types)
        mime_type: Mime,
        // Bytes instead of Vec<u8> so parsed responses can share the buffer
        // with ParsedResponse.raw instead of holding a second copy
        content: Bytes,
    },
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
