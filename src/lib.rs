use bytes::Bytes;
use futures::{
    channel::mpsc::{unbounded, UnboundedSender},
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
        for (f, l) in self.fields.clone() {
            sl = sl.for_field(f, l);
        }
        sl
    }
}

/* ---------- Constraint ---------- */

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
                .cloned()
                .map(Cow::Owned)
                .collect();
            c = c.allowed_fields(v);
        }
        c
    }
}

/* ---------- Stream helper ---------- */

pub struct ChunkStream {
    rx: Arc<AsyncMutex<Option<futures::channel::mpsc::UnboundedReceiver<Bytes>>>>,
}

impl Stream for ChunkStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Coba dapatkan lock tanpa blocking
        match self.rx.try_lock() {
            Ok(mut guard) => {
                if let Some(ref mut rx) = *guard {
                    // Pin receiver dan poll
                    Pin::new(rx).poll_next(cx).map(|opt| opt.map(Ok))
                } else {
                    Poll::Ready(None)
                }
            }
            Err(_) => Poll::Pending, // Jika tidak dapat lock, pending
        }
    }
}

/* ---------- MultipartParser ---------- */

#[pyclass]
pub struct MultipartParser {
    tx: Arc<AsyncMutex<Option<UnboundedSender<Bytes>>>>,
    rx: Arc<AsyncMutex<Option<futures::channel::mpsc::UnboundedReceiver<Bytes>>>>, // Tambahan rx
    boundary: String,
    constraints: Option<ConstraintWrapper>,
    multipart: Arc<AsyncMutex<Option<Multipart<'static>>>>, // Persistent Multipart
}

#[pymethods]
impl MultipartParser {
    #[new]
    #[pyo3(signature = (boundary, constraints=None))]
    pub fn new(_py: Python, boundary: &str, constraints: Option<ConstraintWrapper>) -> Self {
        let (tx, rx) = unbounded();
        MultipartParser {
            tx: Arc::new(AsyncMutex::new(Some(tx))),
            rx: Arc::new(AsyncMutex::new(Some(rx))),
            boundary: boundary.to_owned(),
            constraints,
            multipart: Arc::new(AsyncMutex::new(None)), // Belum dibuat di sini
        }
    }

    /// Feed raw bytes into the parser.
    pub fn feed<'py>(&self, data: &Bound<'py, PyBytes>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let tx = self.tx.clone();
        let bytes = Bytes::copy_from_slice(data.as_bytes());
        let len = bytes.len();
        future_into_py(py, async move {
            let tx = tx.lock().await;
            if let Some(ref tx) = *tx {
                tx.unbounded_send(bytes)
                    .map_err(|e| PyRuntimeError::new_err(format!("Feed error: {e}")))?;
                Ok(len)
            } else {
                Err(PyRuntimeError::new_err("Parser already closed"))
            }
        })
    }

    /// Signal end-of-stream.
    pub fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let tx = self.tx.clone();
        let rx = self.rx.clone();
        let boundary = self.boundary.clone();
        let constraints = self.constraints.clone();
        let multipart = self.multipart.clone();
        future_into_py(py, async move {
            let mut tx_guard = tx.lock().await;
            if tx_guard.is_none() {
                return Err(PyRuntimeError::new_err("Parser already closed"));
            }
            tx_guard.take();
            // Buat instance Multipart di sini
            let stream = ChunkStream { rx };
            let cons = constraints.as_ref().map(|c| c.to_constraints());
            let mp = match cons {
                Some(c) => Multipart::with_constraints(stream, &boundary, c),
                None => Multipart::new(stream, &boundary),
            };
            *multipart.lock().await = Some(mp);
            Ok(true)
        })
    }

    /// Async generator over fields.
    pub fn next_field<'py>(
        slf: PyRefMut<'py, Self>,
        py: Python<'py>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let multipart = slf.multipart.clone();
        future_into_py(py, async move {
            // Ambil instance multipart dari Option, lalu lepas lock
            let multipart_instance_opt = {
                let mut guard = multipart.lock().await;
                guard.take()
            };
            if let Some(mut multipart_instance) = multipart_instance_opt {
                let next = multipart_instance.next_field().await;
                match next {
                    Ok(Some(field)) => {
                        // Proses field seperti biasa
                        {
                            let mut guard = multipart.lock().await;
                            *guard = Some(multipart_instance);
                        }
                        let name = field.name().map(|s| s.to_owned());
                        let filename = field.file_name().map(|s| s.to_owned());
                        let content_type = field.content_type().map(|m| m.to_string().to_owned());
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
                        // Tidak ada field lagi
                        Ok(None)
                    }
                    Err(e) => {
                        // Tangani error
                        Err(PyRuntimeError::new_err(format!("{e}")))
                    }
                }
            } else {
                Ok(None)
            }
        })
    }
}

/* ---------- MultipartField ---------- */

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

    pub fn __aiter__(slf: PyRefMut<'_, Self>) -> PyResult<Py<MultipartField>> {
        Ok(slf.into())
    }

    pub fn __anext__<'py>(
        slf: PyRefMut<'py, Self>,
        py: Python<'py>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let field = slf.field.clone();
        future_into_py(py, async move {
            let field_opt = {
                let mut guard = field.lock().await;
                guard.take()
            };
            if let Some(mut field_instance) = field_opt {
                match field_instance.chunk().await {
                    Ok(Some(chunk)) => {
                        // Kembalikan field ke Arc<Mutex<Option<...>>>
                        {
                            let mut guard = field.lock().await;
                            *guard = Some(field_instance);
                        }
                        Ok(Some(chunk.to_vec().to_owned()))
                    }
                    Ok(None) => Err(PyStopAsyncIteration::new_err("No more chunks")),
                    Err(e) => Err(PyRuntimeError::new_err(e.to_string())),
                }
            } else {
                Err(PyStopAsyncIteration::new_err("No more chunks"))
            }
        })
    }
}

/* ---------- utility ---------- */

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
