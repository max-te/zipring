#[derive(Debug, Clone, Copy)]
pub enum HttpStatus {
    NotFound,
    BadRequest,
    UriTooLong,
    HeaderTooLong,
    MethodNotAllowed,
}

impl HttpStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            HttpStatus::NotFound => "404 Not Found",
            HttpStatus::BadRequest => "400 Bad Request",
            HttpStatus::UriTooLong => "414 URI Too Long",
            HttpStatus::HeaderTooLong => "431 Request Header Fields Too Large",
            HttpStatus::MethodNotAllowed => "405 Method Not Allowed",
        }
    }
}
