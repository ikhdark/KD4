use bytes::Bytes;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

/// A JSON request body serialized once into reference-counted bytes.
///
/// Clones share the encoded allocation. Internally, the body can also hold the
/// final compressed wire bytes without retaining a second copy for logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedJsonBody {
    bytes: Bytes,
    prepared: bool,
    generated_content_encoding: Option<HeaderValue>,
}

impl EncodedJsonBody {
    /// Serializes `value` into a reusable JSON body.
    pub fn encode<T: Serialize + ?Sized>(value: &T) -> Result<Self, serde_json::Error> {
        serde_json::to_vec(value).map(|bytes| Self {
            bytes: Bytes::from(bytes),
            prepared: false,
            generated_content_encoding: None,
        })
    }

    /// Returns the encoded bytes currently stored by this body.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns a reference-counted view of the encoded JSON without copying it.
    pub fn shared_bytes(&self) -> Bytes {
        self.bytes.clone()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RequestCompression {
    #[default]
    None,
    Zstd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestBody {
    Json(Value),
    EncodedJson(EncodedJsonBody),
    Raw(Bytes),
    /// A builder serialization failure retained until dispatch rejects the request.
    InvalidJson(String),
}

impl RequestBody {
    pub fn json(&self) -> Option<&Value> {
        match self {
            Self::Json(value) => Some(value),
            Self::EncodedJson(_) | Self::Raw(_) | Self::InvalidJson(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedRequestBody {
    pub headers: HeaderMap,
    pub body: Option<Bytes>,
}

impl PreparedRequestBody {
    pub fn body_bytes(&self) -> Bytes {
        self.body.clone().unwrap_or_default()
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    pub url: String,
    pub headers: HeaderMap,
    pub body: Option<RequestBody>,
    pub compression: RequestCompression,
    pub timeout: Option<Duration>,
    /// Receive bound for unary success bodies; never applies to streaming output.
    pub max_response_bytes: Option<usize>,
}

impl Request {
    pub fn new(method: Method, url: String) -> Self {
        Self {
            method,
            url,
            headers: HeaderMap::new(),
            body: None,
            compression: RequestCompression::None,
            timeout: None,
            max_response_bytes: None,
        }
    }

    pub fn with_json<T: Serialize>(mut self, body: &T) -> Self {
        self.clear_prepared_body_headers();
        self.body = Some(match serde_json::to_value(body) {
            Ok(value) => RequestBody::Json(value),
            Err(error) => RequestBody::InvalidJson(error.to_string()),
        });
        self
    }

    pub fn with_raw_body(mut self, body: impl Into<Bytes>) -> Self {
        self.clear_prepared_body_headers();
        self.body = Some(RequestBody::Raw(body.into()));
        self
    }

    fn clear_prepared_body_headers(&mut self) {
        if let Some(RequestBody::EncodedJson(body)) = &self.body
            && body.prepared
        {
            if let Some(generated) = &body.generated_content_encoding {
                let mut encodings = self
                    .headers
                    .get_all(http::header::CONTENT_ENCODING)
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>();
                if let Some(index) = encodings.iter().position(|value| value == generated) {
                    encodings.remove(index);
                    self.headers.remove(http::header::CONTENT_ENCODING);
                    for encoding in encodings {
                        self.headers
                            .append(http::header::CONTENT_ENCODING, encoding);
                    }
                }
            }
            self.headers.remove(http::header::CONTENT_LENGTH);
        }
    }

    pub fn with_compression(mut self, compression: RequestCompression) -> Self {
        self.compression = compression;
        self
    }

    /// Returns the exact number of body bytes when the request has already
    /// been prepared for dispatch.
    pub fn prepared_body_len(&self) -> Option<u64> {
        let len = match self.body.as_ref()? {
            RequestBody::EncodedJson(body) if body.prepared => body.bytes.len(),
            RequestBody::Raw(body) if self.compression == RequestCompression::None => body.len(),
            RequestBody::Json(_)
            | RequestBody::EncodedJson(_)
            | RequestBody::Raw(_)
            | RequestBody::InvalidJson(_) => {
                return None;
            }
        };
        u64::try_from(len).ok()
    }

    /// Prepares the body once and stores the exact bytes that will be sent.
    ///
    /// Cloning the returned request shares the body bytes, so retry attempts do
    /// not repeat JSON serialization or compression. Request-signing auth also
    /// sees the same final headers and bytes that the transport will send.
    pub fn into_prepared(mut self) -> Result<Self, String> {
        if let Some(RequestBody::EncodedJson(body)) = &self.body
            && body.prepared
        {
            self.prepare_body_for_send()?;
            return Ok(self);
        }
        if let Some(RequestBody::Json(body)) = &self.body {
            self.body = Some(RequestBody::EncodedJson(
                EncodedJsonBody::encode(body).map_err(|err| err.to_string())?,
            ));
        }
        let is_json = matches!(
            self.body,
            Some(RequestBody::Json(_) | RequestBody::EncodedJson(_))
        );
        let prepared = self.prepare_body_for_send()?;
        let generated_content_encoding = (self.compression != RequestCompression::None)
            .then(|| {
                prepared
                    .headers
                    .get(http::header::CONTENT_ENCODING)
                    .cloned()
            })
            .flatten();
        self.headers = prepared.headers;
        self.body = match (is_json, prepared.body) {
            (true, Some(bytes)) => Some(RequestBody::EncodedJson(EncodedJsonBody {
                bytes,
                prepared: true,
                generated_content_encoding,
            })),
            (false, Some(body)) => Some(RequestBody::Raw(body)),
            (_, None) => None,
        };
        self.compression = RequestCompression::None;
        Ok(self)
    }

    /// Convert the request body into the exact bytes that will be sent.
    ///
    /// Auth schemes such as AWS SigV4 need to sign the final body bytes, including
    /// compression and content headers. Calling this method does not mutate the
    /// request.
    pub fn prepare_body_for_send(&self) -> Result<PreparedRequestBody, String> {
        let headers = self.headers.clone();
        match self.body.as_ref() {
            Some(RequestBody::Raw(raw_body)) => {
                if self.compression != RequestCompression::None {
                    return Err("request compression cannot be used with raw bodies".to_string());
                }
                Ok(PreparedRequestBody {
                    headers,
                    body: Some(raw_body.clone()),
                })
            }
            Some(RequestBody::Json(body)) => {
                let body = EncodedJsonBody::encode(body).map_err(|err| err.to_string())?;
                self.prepare_encoded_json(headers, &body)
            }
            Some(RequestBody::EncodedJson(body)) => self.prepare_encoded_json(headers, body),
            Some(RequestBody::InvalidJson(error)) => Err(error.clone()),
            None => Ok(PreparedRequestBody {
                headers,
                body: None,
            }),
        }
    }

    fn prepare_encoded_json(
        &self,
        mut headers: HeaderMap,
        body: &EncodedJsonBody,
    ) -> Result<PreparedRequestBody, String> {
        if body.prepared {
            if self.compression != RequestCompression::None {
                return Err("cannot change compression of an already prepared body".to_string());
            }
            return Ok(PreparedRequestBody {
                headers,
                body: Some(body.bytes.clone()),
            });
        }

        let bytes = if self.compression != RequestCompression::None {
            if headers.contains_key(http::header::CONTENT_ENCODING) {
                return Err(
                    "request compression was requested but content-encoding is already set"
                        .to_string(),
                );
            }

            let pre_compression_bytes = body.bytes.len();
            let compression_start = std::time::Instant::now();
            let (compressed, content_encoding) = match self.compression {
                RequestCompression::None => unreachable!("guarded by compression != None"),
                RequestCompression::Zstd => (
                    zstd::stream::encode_all(std::io::Cursor::new(body.as_bytes()), 3)
                        .map_err(|err| err.to_string())?,
                    HeaderValue::from_static("zstd"),
                ),
            };
            let post_compression_bytes = compressed.len();
            let compression_duration = compression_start.elapsed();

            headers.insert(http::header::CONTENT_ENCODING, content_encoding);
            if headers.contains_key(http::header::CONTENT_LENGTH) {
                headers.insert(
                    http::header::CONTENT_LENGTH,
                    HeaderValue::from(post_compression_bytes),
                );
            }

            tracing::debug!(
                pre_compression_bytes,
                post_compression_bytes,
                compression_duration_ms = compression_duration.as_millis(),
                "Compressed request body with zstd"
            );

            Bytes::from(compressed)
        } else {
            body.bytes.clone()
        };

        if !headers.contains_key(http::header::CONTENT_TYPE) {
            headers.insert(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
        }

        Ok(PreparedRequestBody {
            headers,
            body: Some(bytes),
        })
    }
}


#[derive(Debug, Clone)]
pub struct Response {
    pub status: http::StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}
