//! Protocol 1: the messages inside each frame.
//!
//! A client sends `hello` first and rejects a reply whose `protocol` or
//! `data_dir` differs from its own. The framing, `hello` and `shutdown` never
//! change between versions, so any `ck` can stop any supervisor.
//!
//! Every message rejects unknown fields. Variants without data are empty
//! structs (`Status {}`), because serde ignores extra fields on unit variants
//! of an internally tagged enum even with `deny_unknown_fields`.

use std::path::PathBuf;

use serde::de::Error as _;
use serde::ser::SerializeStruct as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::ErrorBody;
use crate::paths::BuildKind;
use crate::service::spec::ServiceId;
use crate::service::state::ServiceState;

/// The protocol version this build speaks.
pub const PROTOCOL: u32 = 1;

/// One request: `{"op": "<name>", …}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// Version check. Reply: [`HelloReply`].
    Hello {
        protocol: u32,
        /// The client's name and version, for the supervisor's log.
        client: String,
    },
    /// Reply: [`StatusReply`]. Reads a snapshot and never waits.
    Status {},
    /// Starts a registered service. Idempotent. Reply: [`ServiceStatus`].
    Start { service: String },
    /// Stops a service. Idempotent. Reply: [`ServiceStatus`].
    Stop { service: String },
    /// Stops every service, then the supervisor. Reply: [`Empty`].
    Shutdown {},
}

/// Reply to `hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloReply {
    pub protocol: u32,
    /// The supervisor's crate version.
    pub version: String,
    pub pid: u32,
    pub build: BuildKind,
    /// The canonical data folder the supervisor serves.
    pub data_dir: PathBuf,
}

/// Reply to `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusReply {
    pub services: Vec<ServiceStatus>,
}

/// One service as `status`, `start` and `stop` report it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceStatus {
    pub id: ServiceId,
    pub state: ServiceState,
    /// The process id while it is starting, running or stopping.
    pub pid: Option<u32>,
    pub port: u16,
    /// When the service entered `state`, in RFC 3339 UTC. `None` until its
    /// first change of state under this supervisor.
    pub since: Option<String>,
    /// True when this supervisor adopted the process from an earlier one (Unix).
    pub adopted: bool,
    /// A protected service is never force-killed.
    pub protected: bool,
    /// The most recent failure, cleared by the next successful start.
    pub last_error: Option<String>,
}

/// A reply with no data: `{}`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

/// One reply frame: `{"ok":true,"data":…}` or `{"ok":false,"error":{…}}`.
///
/// The client knows which request it sent, so it reads the matching `T`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response<T> {
    Ok(T),
    Err(ErrorBody),
}

impl<T> Response<T> {
    /// The reply as a `Result`.
    pub fn into_result(self) -> Result<T, ErrorBody> {
        match self {
            Self::Ok(data) => Ok(data),
            Self::Err(error) => Err(error),
        }
    }
}

impl<T: Serialize> Serialize for Response<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut out = serializer.serialize_struct("Response", 2)?;
        match self {
            Self::Ok(data) => {
                out.serialize_field("ok", &true)?;
                out.serialize_field("data", data)?;
            }
            Self::Err(error) => {
                out.serialize_field("ok", &false)?;
                out.serialize_field("error", error)?;
            }
        }
        out.end()
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Response<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire<T> {
            ok: bool,
            data: Option<T>,
            error: Option<ErrorBody>,
        }

        match Wire::<T>::deserialize(deserializer)? {
            Wire {
                ok: true,
                data: Some(data),
                error: None,
            } => Ok(Self::Ok(data)),
            Wire {
                ok: false,
                data: None,
                error: Some(error),
            } => Ok(Self::Err(error)),
            _ => Err(D::Error::custom(
                "a reply has `data` when `ok` is true and only `error` when it is false",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;

    #[test]
    fn requests_reject_unknown_ops_and_fields() {
        let accepted = [
            r#"{"op":"hello","protocol":1,"client":"ck 0.1.0"}"#,
            r#"{"op":"status"}"#,
            r#"{"op":"start","service":"web"}"#,
            r#"{"op":"stop","service":"web"}"#,
            r#"{"op":"shutdown"}"#,
        ];
        for text in accepted {
            assert!(
                serde_json::from_str::<Request>(text).is_ok(),
                "should accept {text}"
            );
        }

        let rejected = [
            r#"{"op":"restart","service":"web"}"#,
            r#"{"service":"web"}"#,
            r#"{"op":"status","extra":1}"#,
            r#"{"op":"shutdown","extra":1}"#,
            r#"{"op":"start","service":"web","program":"/bin/sh"}"#,
            r#"{"op":"hello","protocol":1}"#,
        ];
        for text in rejected {
            assert!(
                serde_json::from_str::<Request>(text).is_err(),
                "should reject {text}"
            );
        }
    }

    #[test]
    fn replies_keep_their_wire_shape() {
        let ok = Response::Ok(Empty {});
        let ok_text = serde_json::to_string(&ok).unwrap();
        assert_eq!(ok_text, r#"{"ok":true,"data":{}}"#);
        assert_eq!(
            serde_json::from_str::<Response<Empty>>(&ok_text).unwrap(),
            ok
        );

        let err: Response<Empty> = Response::Err(ErrorBody {
            code: ErrorCode::PortInUse,
            message: "port 8900 is already in use".into(),
        });
        let err_text = serde_json::to_string(&err).unwrap();
        assert_eq!(
            err_text,
            r#"{"ok":false,"error":{"code":"port_in_use","message":"port 8900 is already in use"}}"#
        );
        assert_eq!(
            serde_json::from_str::<Response<Empty>>(&err_text).unwrap(),
            err
        );

        let hello = Response::Ok(HelloReply {
            protocol: PROTOCOL,
            version: "0.1.0".into(),
            pid: 42,
            build: BuildKind::Debug,
            data_dir: PathBuf::from("/data/ck-dev"),
        });
        let hello_text = serde_json::to_string(&hello).unwrap();
        assert_eq!(
            serde_json::from_str::<Response<HelloReply>>(&hello_text).unwrap(),
            hello
        );

        let inconsistent = [
            r#"{"ok":true}"#,
            r#"{"ok":false,"data":{}}"#,
            r#"{"ok":true,"data":{},"error":{"code":"busy","message":"m"}}"#,
            r#"{"ok":true,"data":{},"extra":1}"#,
            r#"{"ok":true,"data":{"extra":1}}"#,
            r#"{"ok":false,"error":{"code":"no_such_code","message":"m"}}"#,
        ];
        for text in inconsistent {
            assert!(
                serde_json::from_str::<Response<Empty>>(text).is_err(),
                "should reject {text}"
            );
        }
    }
}
