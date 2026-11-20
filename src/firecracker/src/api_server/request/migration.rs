// Copyright 2026 VPSnet. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::Deserialize;
use vmm::migration::{MigrationReceiveRoundParams, MigrationSendRoundParams};
use vmm::rpc_interface::VmmAction;

use super::super::parsed_request::{ParsedRequest, RequestError};
use super::{Body, Method, StatusCode};

/// The descriptor a migration request carries is passed with SCM_RIGHTS on the API socket.
/// `fd` in the body, when present, is its index among the descriptors of the request: 0.
fn check_fd_index(fd: Option<u32>) -> Result<(), RequestError> {
    match fd {
        None | Some(0) => Ok(()),
        Some(other) => Err(RequestError::Generic(
            StatusCode::BadRequest,
            format!(
                "fd {other}: pass exactly one descriptor with SCM_RIGHTS on this request; `fd` \
                 is its index and can only be 0"
            ),
        )),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SendRoundBody {
    #[serde(default)]
    first: bool,
    #[serde(default)]
    fd: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiveRoundBody {
    #[serde(default)]
    fd: Option<u32>,
}

pub(crate) fn parse_get_migration(operation: Option<&str>) -> Result<ParsedRequest, RequestError> {
    match operation {
        None => Ok(ParsedRequest::new_sync(VmmAction::GetMigration)),
        Some("memory-digest") => Ok(ParsedRequest::new_sync(VmmAction::GetMigrationMemoryDigest)),
        Some(other) => Err(RequestError::InvalidPathMethod(
            format!("/migration/{other}"),
            Method::Get,
        )),
    }
}

pub(crate) fn parse_put_migration(
    body: &Body,
    operation: Option<&str>,
) -> Result<ParsedRequest, RequestError> {
    match operation {
        Some("send-round") => {
            let body = serde_json::from_slice::<SendRoundBody>(body.raw())?;
            check_fd_index(body.fd)?;
            Ok(ParsedRequest::new_sync(VmmAction::MigrationSendRound(
                MigrationSendRoundParams {
                    first: body.first,
                    stream: None,
                },
            )))
        }
        Some("receive-round") => {
            let body = serde_json::from_slice::<ReceiveRoundBody>(body.raw())?;
            check_fd_index(body.fd)?;
            Ok(ParsedRequest::new_sync(VmmAction::MigrationReceiveRound(
                MigrationReceiveRoundParams { stream: None },
            )))
        }
        Some(other) => Err(RequestError::InvalidPathMethod(
            format!("/migration/{other}"),
            Method::Put,
        )),
        None => Err(RequestError::Generic(
            StatusCode::BadRequest,
            "Missing migration operation.".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_server::parsed_request::tests::vmm_action_from_request;

    #[test]
    fn test_parse_get_migration() {
        assert_eq!(
            vmm_action_from_request(parse_get_migration(None).unwrap()),
            VmmAction::GetMigration
        );
        assert_eq!(
            vmm_action_from_request(parse_get_migration(Some("memory-digest")).unwrap()),
            VmmAction::GetMigrationMemoryDigest
        );
        parse_get_migration(Some("digest")).unwrap_err();
    }

    #[test]
    fn test_parse_put_send_round() {
        for (body, first) in [
            (r#"{"first": true}"#, true),
            (r#"{"first": false, "fd": 0}"#, false),
            ("{}", false),
        ] {
            assert_eq!(
                vmm_action_from_request(
                    parse_put_migration(&Body::new(body), Some("send-round")).unwrap()
                ),
                VmmAction::MigrationSendRound(MigrationSendRoundParams {
                    first,
                    stream: None
                })
            );
        }
        parse_put_migration(&Body::new(r#"{"fd": 3}"#), Some("send-round")).unwrap_err();
        parse_put_migration(&Body::new(r#"{"frist": true}"#), Some("send-round")).unwrap_err();
        parse_put_migration(&Body::new("{}"), Some("send")).unwrap_err();
        for body in ["{}", r#"{"fd": 0}"#] {
            assert_eq!(
                vmm_action_from_request(
                    parse_put_migration(&Body::new(body), Some("receive-round")).unwrap()
                ),
                VmmAction::MigrationReceiveRound(MigrationReceiveRoundParams { stream: None })
            );
        }
        parse_put_migration(&Body::new(r#"{"first": true}"#), Some("receive-round")).unwrap_err();
        parse_put_migration(&Body::new("{}"), None).unwrap_err();
    }
}
