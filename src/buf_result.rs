/// Records `err` in the current span, then returns it along with `buf`.
macro_rules! bail_traced {
    ($err:expr, $buf:expr) => {{
        let err = $err;
        ::tracing::error!(error = %err);
        return ::compio::BufResult(Err(err), $buf);
    }};
}

/// Like [`compio::buf::buf_try!`], but records the error before bailing.
macro_rules! buf_try_traced {
    ($e:expr) => {
        match $e {
            ::compio::BufResult(Ok(res), buf) => (res, buf),
            ::compio::BufResult(Err(err), buf) => $crate::buf_result::bail_traced!(err, buf),
        }
    };
    ($e:expr, $buf:expr) => {{
        let buf = $buf;
        match $e {
            Ok(res) => (res, buf),
            Err(err) => $crate::buf_result::bail_traced!(err, buf),
        }
    }};
}

pub(crate) use {bail_traced, buf_try_traced};
