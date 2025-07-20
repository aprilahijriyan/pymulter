use bytes::Bytes;
use futures::{
    channel::mpsc::{unbounded, UnboundedSender, UnboundedReceiver},
    Stream,
};
use multer::{Constraints, Multipart, SizeLimit};
use pyo3::{
    exceptions::{PyRuntimeError, PyStopAsyncIteration, PyValueError},
    prelude::*,
    types::{PyBytes, PyDict, PyList},
};
use pyo3_async_runtimes::tokio::future_into_py;
use std::{
    borrow::Cow,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::Mutex as AsyncMutex;

/* ---------- SizeLimit ---------- */

/// Configuration for size limits in multipart parsing.
/// 
/// This class allows you to set various size limits for multipart parsing:
/// - whole_stream: Total size limit for the entire stream
/// - per_field: Size limit for individual fields
/// - fields: Size limits for specific field names
/// 
/// Example:
///     size_limit = SizeLimit(
///         whole_stream=1024*1024,  # 1MB total
///         per_field=100*1024,      # 100KB per field
///         fields={"file": 500*1024}  # 500KB for 'file' field
///     )
#[pyclass(name = "SizeLimit")]
#[derive(Clone)]
pub struct SizeLimitWrapper {
    whole_stream: Option<u64>,
    per_field: Option<u64>,
    fields: std::collections::HashMap<String, u64>,
}

#[pymethods]
impl SizeLimitWrapper {
    #[new]
    #[pyo3(signature = (whole_stream=None, per_field=None, fields=None))]
    pub fn new(
        whole_stream: Option<u64>,
        per_field: Option<u64>,
        fields: Option<Bound<'_, PyDict>>,
    ) -> Self {
        let mut map = std::collections::HashMap::new();
        if let Some(dict) = fields {
            for (k, v) in dict.iter() {
                if let (Ok(field), Ok(limit)) = (k.extract::<String>(), v.extract::<u64>()) {
                    map.insert(field, limit);
                }
            }
        }
        SizeLimitWrapper {
            whole_stream,
            per_field,
            fields: map,
        }
    }

    #[getter]
    pub fn whole_stream(&self) -> Option<u64> {
        self.whole_stream
    }

    #[getter]
    pub fn per_field(&self) -> Option<u64> {
        self.per_field
    }

    #[getter]
    pub fn fields(&self, py: Python) -> PyObject {
        let dict = PyDict::new(py);
        for (k, v) in &self.fields {
            dict.set_item(k, v).unwrap();
        }
        dict.into()
    }
}

impl SizeLimitWrapper {
    pub fn to_size_limit(&self) -> SizeLimit {
        let mut sl = SizeLimit::new();
        if let Some(ws) = self.whole_stream {
            sl = sl.whole_stream(ws);
        }
        if let Some(pf) = self.per_field {
            sl = sl.per_field(pf);
        }
        for (f, l) in &self.fields {
            sl = sl.for_field(f.clone(), *l);
        }
        sl
    }
}

/* ---------- Constraint ---------- */

/// Configuration for multipart parsing constraints.
/// 
/// This class allows you to set constraints for multipart parsing:
/// - size_limit: Size limit configuration
/// - allowed_fields: List of allowed field names
/// 
/// Example:
///     constraint = Constraint(
///         size_limit=SizeLimit(whole_stream=1024*1024),
///         allowed_fields=["file", "description"]
///     )
#[pyclass(name = "Constraint")]
#[derive(Clone)]
pub struct ConstraintWrapper {
    size_limit: Option<SizeLimitWrapper>,
    allowed_fields: Vec<String>,
}

#[pymethods]
impl ConstraintWrapper {
    #[new]
    #[pyo3(signature = (size_limit=None, allowed_fields=None))]
    pub fn new(
        size_limit: Option<Bound<'_, SizeLimitWrapper>>,
        allowed_fields: Option<Bound<'_, PyList>>,
    ) -> PyResult<Self> {
        let allowed_fields = if let Some(list) = allowed_fields {
            list.iter()
                .filter_map(|i| i.extract::<String>().ok())
                .collect()
        } else {
            Vec::new()
        };
        let size_limit = size_limit.map(|sl| sl.extract().unwrap());
        Ok(ConstraintWrapper {
            size_limit,
            allowed_fields,
        })
    }

    #[getter]
    pub fn size_limit(&self) -> Option<SizeLimitWrapper> {
        self.size_limit.clone()
    }

    #[getter]
    pub fn allowed_fields(&self) -> Vec<String> {
        self.allowed_fields.clone()
    }
}

impl ConstraintWrapper {
    pub fn to_constraints(&self) -> Constraints {
        let mut c = Constraints::new();
        if let Some(ref sl) = self.size_limit {
            c = c.size_limit(sl.to_size_limit());
        }
        if !self.allowed_fields.is_empty() {
            let v: Vec<Cow<'static, str>> = self
                .allowed_fields
                .iter()
                .map(|s| Cow::Owned(s.clone()))
                .collect();
            c = c.allowed_fields(v);
        }
        c
    }
}

/* ---------- Stream helper ---------- */

pub struct ChunkStream {
    rx: Arc<AsyncMutex<UnboundedReceiver<Bytes>>>,
}

impl Stream for ChunkStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Coba dapatkan lock tanpa blocking
        match self.rx.try_lock() {
            Ok(mut guard) => {
                // Pin receiver dan poll
                Pin::new(&mut *guard).poll_next(cx).map(|opt| opt.map(Ok))
            }
            Err(_) => Poll::Pending, // Jika tidak dapat lock, pending
        }
    }
}

/* ---------- Parser State ---------- */

#[derive(Debug)]
enum ParserState {
    Active(UnboundedSender<Bytes>),
    Closed,
}

#[derive(Debug)]
enum MultipartState {
    NotInitialized,
    Active(Multipart<'static>),
    Exhausted,
}

/* ---------- MultipartParser ---------- */

/// A parser for multipart/form-data streams.
/// 
/// This class provides an interface to parse multipart/form-data streams
/// asynchronously. It accepts raw bytes through the async `feed()` method and
/// provides access to parsed fields through the async `next_field()` method.
#[pyclass]
pub struct MultipartParser {
    state: Arc<AsyncMutex<ParserState>>,
    rx: Arc<AsyncMutex<UnboundedReceiver<Bytes>>>,
    boundary: String,
    constraints: Option<ConstraintWrapper>,
    multipart_state: Arc<AsyncMutex<MultipartState>>,
}

#[pymethods]
impl MultipartParser {
    #[new]
    #[pyo3(signature = (boundary, constraints=None))]
    pub fn new(_py: Python, boundary: &str, constraints: Option<ConstraintWrapper>) -> Self {
        let (tx, rx) = unbounded();
        MultipartParser {
            state: Arc::new(AsyncMutex::new(ParserState::Active(tx))),
            rx: Arc::new(AsyncMutex::new(rx)),
            boundary: boundary.to_owned(),
            constraints,
            multipart_state: Arc::new(AsyncMutex::new(MultipartState::NotInitialized)),
        }
    }

    /// Feed raw bytes into the parser (async function).
    /// 
    /// This async method accepts raw bytes and sends them to the internal parser buffer.
    /// The parser will accumulate these bytes until `close()` is called.
    /// 
    /// Args:
    ///     data: Raw bytes to feed into the parser
    /// 
    /// Returns:
    ///     Number of bytes successfully fed into the parser
    /// 
    /// Raises:
    ///     RuntimeError: If the parser has already been closed
    pub fn feed<'py>(&self, data: &Bound<'py, PyBytes>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let state = self.state.clone();
        let bytes = Bytes::copy_from_slice(data.as_bytes());
        let len = bytes.len();
        future_into_py(py, async move {
            let mut guard = state.lock().await;
            match &mut *guard {
                ParserState::Active(tx) => {
                    tx.unbounded_send(bytes)
                        .map_err(|e| PyRuntimeError::new_err(format!("Feed error: {e}")))?;
                    Ok(len)
                }
                ParserState::Closed => {
                    Err(PyRuntimeError::new_err("Parser already closed"))
                }
            }
        })
    }

    /// Signal end-of-stream and initialize the multipart parser (async function).
    /// 
    /// This async method closes the input stream and initializes the multipart parser
    /// with the accumulated data. After calling this method, you can use
    /// `next_field()` to iterate over the parsed fields.
    /// 
    /// Returns:
    ///     True if the parser was successfully closed and initialized
    /// 
    /// Raises:
    ///     RuntimeError: If the parser has already been closed
    pub fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let state = self.state.clone();
        let rx = self.rx.clone();
        let boundary = self.boundary.clone();
        let constraints = self.constraints.clone();
        let multipart_state = self.multipart_state.clone();
        future_into_py(py, async move {
            // Close the sender
            let mut state_guard = state.lock().await;
            if let ParserState::Active(_) = *state_guard {
                *state_guard = ParserState::Closed;
            } else {
                return Err(PyRuntimeError::new_err("Parser already closed"));
            }
            // Initialize multipart
            let stream = ChunkStream { rx };
            let cons = constraints.as_ref().map(|c| c.to_constraints());
            let mp = match cons {
                Some(c) => Multipart::with_constraints(stream, &boundary, c),
                None => Multipart::new(stream, &boundary),
            };
            
            let mut mp_guard = multipart_state.lock().await;
            *mp_guard = MultipartState::Active(mp);
            drop(state_guard);
            Ok(true)
        })
    }

    /// Get the next field from the multipart data (async function).
    /// 
    /// This async method returns the next field from the parsed multipart data.
    /// Each field contains metadata (name, filename, content_type, headers)
    /// and can be iterated over to get the field's content as chunks.
    /// 
    /// Returns:
    ///     The next field as a MultipartField object, or None if no more fields
    /// 
    /// Raises:
    ///     RuntimeError: If the parser is not initialized (call close() first)
    ///     RuntimeError: If there's an error parsing the multipart data
    pub fn next_field<'py>(
        slf: PyRefMut<'py, Self>,
        py: Python<'py>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let multipart_state = slf.multipart_state.clone();
        future_into_py(py, async move {
            let mut guard = multipart_state.lock().await;
            match &mut *guard {
                MultipartState::Active(multipart_instance) => {
                    let next = multipart_instance.next_field().await;
                    match next {
                        Ok(Some(field)) => {
                            let name = field.name().map(|s| s.to_owned());
                            let filename = field.file_name().map(|s| s.to_owned());
                            let content_type = field.content_type().map(|m| m.to_string());
                            let headers = field
                                .headers()
                                .iter()
                                .map(|(k, v)| {
                                    (
                                        String::from_utf8_lossy(k.as_ref()).into_owned(),
                                        String::from_utf8_lossy(v.as_ref()).into_owned(),
                                    )
                                })
                                .collect();

                            let py_field = Python::with_gil(|py| {
                                Py::new(py, MultipartField {
                                    name,
                                    filename,
                                    content_type,
                                    headers,
                                    field: Arc::new(AsyncMutex::new(Some(field))),
                                })
                            })?;
                            Ok(Some(py_field))
                        }
                        Ok(None) => {
                            *guard = MultipartState::Exhausted;
                            Ok(None)
                        }
                        Err(e) => {
                            Err(PyRuntimeError::new_err(format!("{e}")))
                        }
                    }
                }
                MultipartState::NotInitialized => {
                    Err(PyRuntimeError::new_err("Parser not initialized. Call close() first."))
                }
                MultipartState::Exhausted => {
                    Ok(None)
                }
            }
        })
    }
}

/* ---------- MultipartField ---------- */

/// A field from a multipart/form-data stream.
/// 
/// This class represents a single field from a multipart/form-data stream.
/// It contains metadata about the field and provides async iteration
/// over the field's content chunks.
/// 
/// The field can be used in async for loops to iterate over its content:
///     async for chunk in field:
///         process_chunk(chunk)
/// 
/// Attributes:
///     name: The field name
///     filename: The filename if this is a file field
///     content_type: The content type of the field
///     headers: Additional headers for this field
#[pyclass]
pub struct MultipartField {
    name: Option<String>,
    filename: Option<String>,
    content_type: Option<String>,
    headers: Vec<(String, String)>,
    field: Arc<AsyncMutex<Option<multer::Field<'static>>>>,
}

#[pymethods]
impl MultipartField {
    #[getter]
    fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    #[getter]
    fn filename(&self) -> Option<&str> {
        self.filename.as_deref()
    }

    #[getter]
    fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    #[getter]
    fn headers(&self) -> Vec<(String, String)> {
        self.headers.clone()
    }

    /// Make the field iterable for async iteration.
    /// 
    /// This method allows the field to be used in async for loops
    /// to iterate over the field's content chunks.
    /// Note: This is a synchronous method that returns an async iterator.
    pub fn __aiter__(slf: PyRefMut<'_, Self>) -> PyResult<Py<MultipartField>> {
        Ok(slf.into())
    }

    /// Get the next chunk of field data (async function).
    /// 
    /// This async method returns the next chunk of data from the field.
    /// When all chunks have been consumed, it raises StopAsyncIteration.
    /// 
    /// Returns:
    ///     The next chunk as bytes, or raises StopAsyncIteration when done
    /// 
    /// Raises:
    ///     StopAsyncIteration: When no more chunks are available
    ///     RuntimeError: If there's an error reading the field data
    pub fn __anext__<'py>(
        slf: PyRefMut<'py, Self>,
        py: Python<'py>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let field = slf.field.clone();
        future_into_py(py, async move {
            let mut guard = field.lock().await;
            match guard.as_mut() {
                Some(field_instance) => {
                    match field_instance.chunk().await {
                        Ok(Some(chunk)) => Ok(Some(chunk.to_vec())),
                        Ok(None) => {
                            // Jika sudah habis, set None agar next iterasi return StopAsyncIteration
                            *guard = None;
                            Err(PyStopAsyncIteration::new_err("No more chunks"))
                        }
                        Err(e) => Err(PyRuntimeError::new_err(e.to_string())),
                    }
                }
                None => Err(PyStopAsyncIteration::new_err("No more chunks")),
            }
        })
    }
}

/* ---------- utility ---------- */

/// Parse the boundary from a Content-Type header.
/// 
/// This function extracts the boundary parameter from a Content-Type header
/// that contains multipart/form-data.
/// 
/// Args:
///     header: The Content-Type header string
/// 
/// Returns:
///     The boundary string
/// 
/// Raises:
///     ValueError: If the header format is invalid
#[pyfunction(name = "parse_boundary")]
pub fn py_parse_boundary(header: &str) -> PyResult<Option<String>> {
    multer::parse_boundary(header)
        .map(|b| Some(b.to_string()))
        .map_err(|e| PyValueError::new_err(e.to_string()))
}

#[pymodule]
fn pymulter(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<SizeLimitWrapper>()?;
    m.add_class::<ConstraintWrapper>()?;
    m.add_class::<MultipartParser>()?;
    m.add_class::<MultipartField>()?;
    m.add_function(wrap_pyfunction!(py_parse_boundary, m)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_size_limit_wrapper_to_size_limit() {
        let mut fields = std::collections::HashMap::new();
        fields.insert("file".to_string(), 500u64);
        let wrapper = SizeLimitWrapper {
            whole_stream: Some(1000),
            per_field: Some(100),
            fields,
        };
        let sl = wrapper.to_size_limit();
        // SizeLimit tidak expose field, tapi kita bisa cek via debug
        let debug = format!("{:?}", sl);
        // println!("{}", debug);
        assert!(debug.contains("whole_stream: 1000"));
        assert!(debug.contains("per_field: 100"));
        assert!(debug.contains("field_map: {\"file\": 500}"));
    }

    #[test]
    fn test_constraint_wrapper_to_constraints() {
        let size_limit = SizeLimitWrapper {
            whole_stream: Some(1000),
            per_field: Some(100),
            fields: std::collections::HashMap::new(),
        };
        let wrapper = ConstraintWrapper {
            size_limit: Some(size_limit.clone()),
            allowed_fields: vec!["file".to_string(), "desc".to_string()],
        };
        let cons = wrapper.to_constraints();
        let debug = format!("{:?}", cons);
        // println!("{}", debug);
        assert!(debug.contains("whole_stream: 1000"));
        assert!(debug.contains("per_field: 100"));
        assert!(debug.contains("allowed_fields: Some([\"file\", \"desc\"])"));
    }
}
