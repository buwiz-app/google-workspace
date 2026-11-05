//! Where Google's APIs are. A build with the `stub` feature can be pointed at
//! the stub Google on loopback; a release can't be pointed anywhere, so the
//! token only ever goes to Google.

/// The API bases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Apis {
    pub gmail: String,
}

/// Names the stub Google, in a `stub` build.
pub const STUB_ENV: &str = "GWS_STUB_BASE";

impl Apis {
    pub fn google() -> Self {
        Self { gmail: crate::gmail::GMAIL_API.to_owned() }
    }

    /// Everything under one base, as the stub serves it.
    pub fn at(base: &str) -> Self {
        let base = base.trim_end_matches('/');
        Self { gmail: format!("{base}/gmail/v1/users/me") }
    }

    /// Google, unless this is a `stub` build and [`STUB_ENV`] names a loopback server.
    pub fn from_env() -> Self {
        #[cfg(feature = "stub")]
        if let Ok(base) = std::env::var(STUB_ENV) {
            if base.starts_with("http://127.0.0.1:") {
                return Self::at(&base);
            }
        }
        Self::google()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs only without the feature: what a release does.
    #[cfg(not(feature = "stub"))]
    #[test]
    fn a_release_build_ignores_the_stub_variable() {
        // the variable is only ever read in a `stub` build: nothing here can name it
        assert_eq!(Apis::from_env(), Apis::google());
        assert!(Apis::google().gmail.starts_with("https://gmail.googleapis.com/"));
    }

    #[test]
    fn the_stub_is_under_one_base() {
        assert_eq!(Apis::at("http://127.0.0.1:9/").gmail, "http://127.0.0.1:9/gmail/v1/users/me");
    }
}
