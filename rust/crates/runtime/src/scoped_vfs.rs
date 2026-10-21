//! Typed result handling for the restricted remote filesystem entry.
use nexus_vfs_client::{proto, NexusVfsClient};
use proto::{scoped_fs_request::Operation, scoped_fs_response::Result as Reply};
use std::io;

pub(crate) fn call(
    client: &NexusVfsClient,
    scope: &[u8],
    token: &str,
    op: Operation,
) -> io::Result<Reply> {
    let reply = client.scoped(op, scope, token)?;
    let error = match &reply {
        Reply::Read(r) => (r.is_error, &r.error_payload),
        Reply::Write(r) | Reply::Rename(r) | Reply::Append(r) => (r.is_error, &r.error_payload),
        Reply::Stat(r) => (r.is_error, &r.error_payload),
        Reply::Readdir(r) => (r.is_error, &r.error_payload),
        Reply::Setattr(r) => (r.is_error, &r.error_payload),
        Reply::Mkdir(r) => (r.is_error, &r.error_payload),
        Reply::Delete(r) => (r.is_error, &r.error_payload),
        Reply::Lock(r) => (r.is_error, &r.error_payload),
        Reply::Unlock(r) => (r.is_error, &r.error_payload),
        Reply::StreamWrite(r) => (r.is_error, &r.error_payload),
        Reply::StreamReadAt(r) => (r.is_error, &r.error_payload),
    };
    if error.0 {
        return Err(nexus_vfs_client::vfs_err(error.1));
    }
    Ok(reply)
}

pub(crate) fn wrong_reply() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "unexpected scoped result type")
}
