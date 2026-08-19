# lean http client based on curl

small blocking http client on top of libcurl. Built for exchanging parameters/files with services (multipart, form-urlencoded, raw bodies) without dealing with curl directly.

## usage

```rust
use http_helper::{Client, Method, Parameter, ParameterType};

let mut client = Client::new("http://localhost:5678?debug=1", Method::POST)?;
client.add_parameter(Parameter::SimpleParameter {
    name: "key".to_owned(),
    value: "value".to_owned(),
    param_type: ParameterType::Body,
});
let response = client.execute()?;
println!("{}", response.status_code);
```

files go in as complex parameters:

```rust
client.add_complex_parameter("report", mime::APPLICATION_PDF, &data)?;
```

one body parameter -> sent raw, multiple simple ones -> form urlencoded, anything with a file in it -> multipart. Query params in the url are parsed and re-encoded, so pass them unencoded.

responses are parsed back into the same Parameter enum (multipart gets split into parts), or use `execute_raw()` if you just want status + headers + bytes.

## features

- `static-curl` / `static-ssl`: passthrough of the curl crate's static linking flags, default is dynamic
