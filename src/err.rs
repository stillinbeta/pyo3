// Copyright (c) 2017-present PyO3 Project and Contributors

use crate::gil::ensure_gil;
use crate::panic::PanicException;
use crate::type_object::PyTypeObject;
use crate::types::PyType;
use crate::{exceptions, ffi};
use crate::{
    AsPyPointer, FromPy, FromPyPointer, IntoPy, IntoPyPointer, Py, PyAny, PyNativeType, PyObject,
    Python, ToBorrowedObject, ToPyObject,
};
use libc::c_int;
use std::ffi::CString;
use std::io;
use std::os::raw::c_char;
use std::ptr::NonNull;

/// Represents a `PyErr` value.
///
/// **Caution:**
///
/// When you construct an instance of `PyErrValue`, we highly recommend to use `from_err_args`
/// method.  If you want to to construct `PyErrValue::ToArgs` directly, please do not forget to
/// call `Python::acquire_gil`.
pub enum PyErrValue {
    None,
    Value(PyObject),
    ToArgs(Box<dyn PyErrArguments>),
    ToObject(Box<dyn ToPyObject>),
}

impl PyErrValue {
    pub fn from_err_args<T: 'static + PyErrArguments>(value: T) -> Self {
        let _ = Python::acquire_gil();
        PyErrValue::ToArgs(Box::new(value))
    }
}

/// Represents a Python exception that was raised.
pub struct PyErr {
    /// The type of the exception. This should be either a `PyClass` or a `PyType`.
    pub ptype: Py<PyType>,

    /// The value of the exception.
    ///
    /// This can be either an instance of `PyObject`, a tuple of arguments to be passed to
    /// `ptype`'s constructor, or a single argument to be passed to `ptype`'s constructor.  Call
    /// `PyErr::to_object()` to get the exception instance in all cases.
    pub pvalue: PyErrValue,

    /// The `PyTraceBack` object associated with the error.
    pub ptraceback: Option<PyObject>,
}

/// Represents the result of a Python call.
pub type PyResult<T> = Result<T, PyErr>;

/// Marker type that indicates an error while downcasting
pub struct PyDowncastError;

/// Helper conversion trait that allows to use custom arguments for exception constructor.
pub trait PyErrArguments {
    /// Arguments for exception
    fn arguments(&self, _: Python) -> PyObject;
}

impl PyErr {
    /// Creates a new PyErr of type `T`.
    ///
    /// `value` can be:
    /// * a tuple: the exception instance will be created using Python `T(*tuple)`
    /// * any other value: the exception instance will be created using Python `T(value)`
    ///
    /// Panics if `T` is not a Python class derived from `BaseException`.
    ///
    /// Example:
    /// ```ignore
    /// return Err(PyErr::new::<exceptions::TypeError, _>("Error message"));
    /// ```
    ///
    /// In most cases, you can use a concrete exception's constructors instead:
    /// the example is equivalent to
    /// ```ignore
    /// return Err(exceptions::TypeError::py_err("Error message"));
    /// return exceptions::TypeError::into("Error message");
    /// ```
    pub fn new<T, V>(value: V) -> PyErr
    where
        T: PyTypeObject,
        V: ToPyObject + 'static,
    {
        let gil = ensure_gil();
        let py = unsafe { gil.python() };

        let ty = T::type_object(py);
        assert_ne!(unsafe { ffi::PyExceptionClass_Check(ty.as_ptr()) }, 0);

        PyErr {
            ptype: ty.into(),
            pvalue: PyErrValue::ToObject(Box::new(value)),
            ptraceback: None,
        }
    }

    /// Constructs a new error, with the usual lazy initialization of Python exceptions.
    ///
    /// `exc` is the exception type; usually one of the standard exceptions
    /// like `exceptions::RuntimeError`.
    /// `args` is the a tuple of arguments to pass to the exception constructor.
    pub fn from_type<A>(exc: &PyType, args: A) -> PyErr
    where
        A: ToPyObject + 'static,
    {
        PyErr {
            ptype: exc.into(),
            pvalue: PyErrValue::ToObject(Box::new(args)),
            ptraceback: None,
        }
    }

    /// Creates a new PyErr of type `T`.
    pub fn from_value<T>(value: PyErrValue) -> PyErr
    where
        T: PyTypeObject,
    {
        let gil = ensure_gil();
        let py = unsafe { gil.python() };

        let ty = T::type_object(py);
        assert_ne!(unsafe { ffi::PyExceptionClass_Check(ty.as_ptr()) }, 0);

        PyErr {
            ptype: ty.into(),
            pvalue: value,
            ptraceback: None,
        }
    }

    /// Creates a new PyErr.
    ///
    /// `obj` must be an Python exception instance, the PyErr will use that instance.
    /// If `obj` is a Python exception type object, the PyErr will (lazily) create a new
    /// instance of that type.
    /// Otherwise, a `TypeError` is created instead.
    pub fn from_instance(obj: &PyAny) -> PyErr {
        let ptr = obj.as_ptr();

        if unsafe { ffi::PyExceptionInstance_Check(ptr) } != 0 {
            PyErr {
                ptype: unsafe {
                    Py::from_borrowed_ptr(obj.py(), ffi::PyExceptionInstance_Class(ptr))
                },
                pvalue: PyErrValue::Value(obj.into()),
                ptraceback: None,
            }
        } else if unsafe { ffi::PyExceptionClass_Check(obj.as_ptr()) } != 0 {
            PyErr {
                ptype: unsafe { Py::from_borrowed_ptr(obj.py(), ptr) },
                pvalue: PyErrValue::None,
                ptraceback: None,
            }
        } else {
            PyErr {
                ptype: exceptions::TypeError::type_object(obj.py()).into(),
                pvalue: PyErrValue::ToObject(Box::new("exceptions must derive from BaseException")),
                ptraceback: None,
            }
        }
    }

    /// Gets whether an error is present in the Python interpreter's global state.
    #[inline]
    pub fn occurred(_: Python) -> bool {
        unsafe { !ffi::PyErr_Occurred().is_null() }
    }

    /// Retrieves the current error from the Python interpreter's global state.
    ///
    /// The error is cleared from the Python interpreter.
    /// If no error is set, returns a `SystemError`.
    ///
    /// If the error fetched is a `PanicException` (which would have originated from a panic in a
    /// pyo3 callback) then this function will resume the panic.
    pub fn fetch(py: Python) -> PyErr {
        unsafe {
            let mut ptype: *mut ffi::PyObject = std::ptr::null_mut();
            let mut pvalue: *mut ffi::PyObject = std::ptr::null_mut();
            let mut ptraceback: *mut ffi::PyObject = std::ptr::null_mut();
            ffi::PyErr_Fetch(&mut ptype, &mut pvalue, &mut ptraceback);

            let err = PyErr::new_from_ffi_tuple(py, ptype, pvalue, ptraceback);

            if ptype == PanicException::type_object(py).as_ptr() {
                let msg: String = PyAny::from_borrowed_ptr_or_opt(py, pvalue)
                    .and_then(|obj| obj.extract().ok())
                    .unwrap_or_else(|| String::from("Unwrapped panic from Python code"));

                eprintln!(
                    "--- PyO3 is resuming a panic after fetching a PanicException from Python. ---"
                );
                eprintln!("Python stack trace below:");
                err.print(py);

                std::panic::resume_unwind(Box::new(msg))
            }

            err
        }
    }

    /// Creates a new exception type with the given name, which must be of the form
    /// `<module>.<ExceptionName>`, as required by `PyErr_NewException`.
    ///
    /// `base` can be an existing exception type to subclass, or a tuple of classes
    /// `dict` specifies an optional dictionary of class variables and methods
    pub fn new_type<'p>(
        _: Python<'p>,
        name: &str,
        base: Option<&PyType>,
        dict: Option<PyObject>,
    ) -> NonNull<ffi::PyTypeObject> {
        let base: *mut ffi::PyObject = match base {
            None => std::ptr::null_mut(),
            Some(obj) => obj.as_ptr(),
        };

        let dict: *mut ffi::PyObject = match dict {
            None => std::ptr::null_mut(),
            Some(obj) => obj.as_ptr(),
        };

        unsafe {
            let null_terminated_name =
                CString::new(name).expect("Failed to initialize nul terminated exception name");

            NonNull::new_unchecked(ffi::PyErr_NewException(
                null_terminated_name.as_ptr() as *mut c_char,
                base,
                dict,
            ) as *mut ffi::PyTypeObject)
        }
    }

    unsafe fn new_from_ffi_tuple(
        py: Python,
        ptype: *mut ffi::PyObject,
        pvalue: *mut ffi::PyObject,
        ptraceback: *mut ffi::PyObject,
    ) -> PyErr {
        // Note: must not panic to ensure all owned pointers get acquired correctly,
        // and because we mustn't panic in normalize().

        let pvalue = if let Some(obj) = PyObject::from_owned_ptr_or_opt(py, pvalue) {
            PyErrValue::Value(obj)
        } else {
            PyErrValue::None
        };

        let ptype = if ptype.is_null() {
            <exceptions::SystemError as PyTypeObject>::type_object(py).into()
        } else {
            Py::from_owned_ptr(py, ptype)
        };

        PyErr {
            ptype,
            pvalue,
            ptraceback: PyObject::from_owned_ptr_or_opt(py, ptraceback),
        }
    }

    /// Prints a standard traceback to `sys.stderr`.
    pub fn print(self, py: Python) {
        self.restore(py);
        unsafe { ffi::PyErr_PrintEx(0) }
    }

    /// Prints a standard traceback to `sys.stderr`, and sets
    /// `sys.last_{type,value,traceback}` attributes to this exception's data.
    pub fn print_and_set_sys_last_vars(self, py: Python) {
        self.restore(py);
        unsafe { ffi::PyErr_PrintEx(1) }
    }

    /// Returns true if the current exception matches the exception in `exc`.
    ///
    /// If `exc` is a class object, this also returns `true` when `self` is an instance of a subclass.
    /// If `exc` is a tuple, all exceptions in the tuple (and recursively in subtuples) are searched for a match.
    pub fn matches<T>(&self, py: Python, exc: T) -> bool
    where
        T: ToBorrowedObject,
    {
        exc.with_borrowed_ptr(py, |exc| unsafe {
            ffi::PyErr_GivenExceptionMatches(self.ptype.as_ptr(), exc) != 0
        })
    }

    /// Returns true if the current exception is instance of `T`.
    pub fn is_instance<T>(&self, py: Python) -> bool
    where
        T: PyTypeObject,
    {
        unsafe {
            ffi::PyErr_GivenExceptionMatches(self.ptype.as_ptr(), T::type_object(py).as_ptr()) != 0
        }
    }

    /// Normalizes the error. This ensures that the exception value is an instance
    /// of the exception type.
    pub fn normalize(&mut self, py: Python) {
        // The normalization helper function involves temporarily moving out of the &mut self,
        // which requires some unsafe trickery:
        unsafe {
            std::ptr::write(self, std::ptr::read(self).into_normalized(py));
        }
        // This is safe as long as normalized() doesn't unwind due to a panic.
    }

    /// Helper function for normalizing the error by deconstructing and reconstructing the `PyErr`.
    /// Must not panic for safety in `normalize()`.
    fn into_normalized(self, py: Python) -> PyErr {
        let PyErr {
            ptype,
            pvalue,
            ptraceback,
        } = self;

        let mut pvalue = match pvalue {
            PyErrValue::None => std::ptr::null_mut(),
            PyErrValue::Value(ob) => ob.into_ptr(),
            PyErrValue::ToArgs(ob) => ob.arguments(py).into_ptr(),
            PyErrValue::ToObject(ob) => ob.to_object(py).into_ptr(),
        };

        let mut ptype = ptype.into_ptr();
        let mut ptraceback = ptraceback.into_ptr();
        unsafe {
            ffi::PyErr_NormalizeException(&mut ptype, &mut pvalue, &mut ptraceback);
            PyErr::new_from_ffi_tuple(py, ptype, pvalue, ptraceback)
        }
    }

    /// Retrieves the exception instance for this error.
    ///
    /// This method takes `mut self` because the error might need
    /// to be normalized in order to create the exception instance.
    fn instance(mut self, py: Python) -> PyObject {
        self.normalize(py);
        match self.pvalue {
            PyErrValue::Value(ref instance) => instance.clone_ref(py),
            _ => py.None(),
        }
    }

    /// Writes the error back to the Python interpreter's global state.
    /// This is the opposite of `PyErr::fetch()`.
    #[inline]
    pub fn restore(self, py: Python) {
        let PyErr {
            ptype,
            pvalue,
            ptraceback,
        } = self;

        let pvalue = match pvalue {
            PyErrValue::None => std::ptr::null_mut(),
            PyErrValue::Value(ob) => ob.into_ptr(),
            PyErrValue::ToArgs(ob) => ob.arguments(py).into_ptr(),
            PyErrValue::ToObject(ob) => ob.to_object(py).into_ptr(),
        };
        unsafe { ffi::PyErr_Restore(ptype.into_ptr(), pvalue, ptraceback.into_ptr()) }
    }

    /// Utility method for proc-macro code
    #[doc(hidden)]
    pub fn restore_and_null<T>(self, py: Python) -> *mut T {
        self.restore(py);
        std::ptr::null_mut()
    }

    /// Utility method for proc-macro code
    #[doc(hidden)]
    pub fn restore_and_minus1(self, py: Python) -> crate::libc::c_int {
        self.restore(py);
        -1
    }

    /// Issues a warning message.
    /// May return a `PyErr` if warnings-as-errors is enabled.
    pub fn warn(py: Python, category: &PyAny, message: &str, stacklevel: i32) -> PyResult<()> {
        let message = CString::new(message)?;
        unsafe {
            error_on_minusone(
                py,
                ffi::PyErr_WarnEx(
                    category.as_ptr(),
                    message.as_ptr(),
                    stacklevel as ffi::Py_ssize_t,
                ),
            )
        }
    }

    pub fn clone_ref(&self, py: Python) -> PyErr {
        let v = match self.pvalue {
            PyErrValue::None => PyErrValue::None,
            PyErrValue::Value(ref ob) => PyErrValue::Value(ob.clone_ref(py)),
            PyErrValue::ToArgs(ref ob) => PyErrValue::Value(ob.arguments(py)),
            PyErrValue::ToObject(ref ob) => PyErrValue::Value(ob.to_object(py)),
        };

        let t = if let Some(ref val) = self.ptraceback {
            Some(val.clone_ref(py))
        } else {
            None
        };
        PyErr {
            ptype: self.ptype.clone_ref(py),
            pvalue: v,
            ptraceback: t,
        }
    }
}

impl std::fmt::Debug for PyErr {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        f.write_str(format!("PyErr {{ type: {:?} }}", self.ptype).as_str())
    }
}

impl FromPy<PyErr> for PyObject {
    fn from_py(other: PyErr, py: Python) -> Self {
        other.instance(py)
    }
}

impl ToPyObject for PyErr {
    fn to_object(&self, py: Python) -> PyObject {
        let err = self.clone_ref(py);
        err.instance(py)
    }
}

impl<'a> IntoPy<PyObject> for &'a PyErr {
    fn into_py(self, py: Python) -> PyObject {
        let err = self.clone_ref(py);
        err.instance(py)
    }
}

/// Convert `PyDowncastError` to Python `TypeError`.
impl std::convert::From<PyDowncastError> for PyErr {
    fn from(_err: PyDowncastError) -> PyErr {
        exceptions::TypeError.into()
    }
}

impl<'p> std::fmt::Debug for PyDowncastError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        f.write_str("PyDowncastError")
    }
}

/// Convert `PyErr` to `io::Error`
impl std::convert::From<PyErr> for std::io::Error {
    fn from(err: PyErr) -> Self {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Python exception: {:?}", err),
        )
    }
}

/// Convert `PyErr` to `PyResult<T>`
impl<T> std::convert::Into<PyResult<T>> for PyErr {
    fn into(self) -> PyResult<T> {
        Err(self)
    }
}

macro_rules! impl_to_pyerr {
    ($err: ty, $pyexc: ty) => {
        impl PyErrArguments for $err {
            fn arguments(&self, py: Python) -> PyObject {
                self.to_string().to_object(py)
            }
        }

        impl std::convert::From<$err> for PyErr {
            fn from(err: $err) -> PyErr {
                PyErr::from_value::<$pyexc>(PyErrValue::from_err_args(err))
            }
        }
    };
}

/// Create `OSError` from `io::Error`
impl std::convert::From<io::Error> for PyErr {
    fn from(err: io::Error) -> PyErr {
        macro_rules! err_value {
            () => {
                PyErrValue::from_err_args(err)
            };
        }
        match err.kind() {
            io::ErrorKind::BrokenPipe => {
                PyErr::from_value::<exceptions::BrokenPipeError>(err_value!())
            }
            io::ErrorKind::ConnectionRefused => {
                PyErr::from_value::<exceptions::ConnectionRefusedError>(err_value!())
            }
            io::ErrorKind::ConnectionAborted => {
                PyErr::from_value::<exceptions::ConnectionAbortedError>(err_value!())
            }
            io::ErrorKind::ConnectionReset => {
                PyErr::from_value::<exceptions::ConnectionResetError>(err_value!())
            }
            io::ErrorKind::Interrupted => {
                PyErr::from_value::<exceptions::InterruptedError>(err_value!())
            }
            io::ErrorKind::NotFound => {
                PyErr::from_value::<exceptions::FileNotFoundError>(err_value!())
            }
            io::ErrorKind::WouldBlock => {
                PyErr::from_value::<exceptions::BlockingIOError>(err_value!())
            }
            io::ErrorKind::TimedOut => PyErr::from_value::<exceptions::TimeoutError>(err_value!()),
            _ => PyErr::from_value::<exceptions::OSError>(err_value!()),
        }
    }
}

impl PyErrArguments for io::Error {
    fn arguments(&self, py: Python) -> PyObject {
        self.to_string().to_object(py)
    }
}

impl<W: 'static + Send + std::fmt::Debug> std::convert::From<std::io::IntoInnerError<W>> for PyErr {
    fn from(err: std::io::IntoInnerError<W>) -> PyErr {
        PyErr::from_value::<exceptions::OSError>(PyErrValue::from_err_args(err))
    }
}

impl<W: Send + std::fmt::Debug> PyErrArguments for std::io::IntoInnerError<W> {
    fn arguments(&self, py: Python) -> PyObject {
        self.to_string().to_object(py)
    }
}

impl PyErrArguments for std::convert::Infallible {
    fn arguments(&self, py: Python) -> PyObject {
        "Infalliable!".to_object(py)
    }
}

impl std::convert::From<std::convert::Infallible> for PyErr {
    fn from(_: std::convert::Infallible) -> PyErr {
        PyErr::new::<exceptions::ValueError, _>("Infalliable!")
    }
}

impl_to_pyerr!(std::array::TryFromSliceError, exceptions::ValueError);
impl_to_pyerr!(std::num::ParseIntError, exceptions::ValueError);
impl_to_pyerr!(std::num::ParseFloatError, exceptions::ValueError);
impl_to_pyerr!(std::num::TryFromIntError, exceptions::ValueError);
impl_to_pyerr!(std::str::ParseBoolError, exceptions::ValueError);
impl_to_pyerr!(std::ffi::IntoStringError, exceptions::UnicodeDecodeError);
impl_to_pyerr!(std::ffi::NulError, exceptions::ValueError);
impl_to_pyerr!(std::str::Utf8Error, exceptions::UnicodeDecodeError);
impl_to_pyerr!(std::string::FromUtf8Error, exceptions::UnicodeDecodeError);
impl_to_pyerr!(std::string::FromUtf16Error, exceptions::UnicodeDecodeError);
impl_to_pyerr!(std::char::DecodeUtf16Error, exceptions::UnicodeDecodeError);
impl_to_pyerr!(std::net::AddrParseError, exceptions::ValueError);

pub fn panic_after_error(_py: Python) -> ! {
    unsafe {
        ffi::PyErr_Print();
    }
    panic!("Python API call failed");
}

/// Returns Ok if the error code is not -1.
#[inline]
pub fn error_on_minusone(py: Python, result: c_int) -> PyResult<()> {
    if result != -1 {
        Ok(())
    } else {
        Err(PyErr::fetch(py))
    }
}

#[cfg(test)]
mod tests {
    use crate::exceptions;
    use crate::panic::PanicException;
    use crate::{PyErr, Python};

    #[test]
    fn set_typeerror() {
        let gil = Python::acquire_gil();
        let py = gil.python();
        let err: PyErr = exceptions::TypeError.into();
        err.restore(py);
        assert!(PyErr::occurred(py));
        drop(PyErr::fetch(py));
    }

    #[test]
    fn fetching_panic_exception_panics() {
        // If -Cpanic=abort is specified, we can't catch panic.
        if option_env!("RUSTFLAGS")
            .map(|s| s.contains("-Cpanic=abort"))
            .unwrap_or(false)
        {
            return;
        }

        let gil = Python::acquire_gil();
        let py = gil.python();
        let err: PyErr = PanicException::py_err("new panic");
        err.restore(py);
        assert!(PyErr::occurred(py));
        let started_unwind =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| PyErr::fetch(py))).is_err();
        assert!(started_unwind);
    }
}
