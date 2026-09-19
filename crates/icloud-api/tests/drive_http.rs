//! `HttpDrive` against a scripted service: request shapes and reply handling.

use std::{
    collections::BTreeMap,
    io::{Read, Write},
};

use httpmock::prelude::*;
use icloud_api::{DeleteMode, Drive, Error, HttpDrive, Node, NodeKind, Session};
use serde_json::json;

fn params() -> BTreeMap<String, String> {
    BTreeMap::from([("clientId".to_owned(), "cid".to_owned()), ("dsid".to_owned(), "42".to_owned())])
}

fn drive(server: &MockServer) -> (HttpDrive, std::sync::Arc<Session>) {
    let session = Session::open("u@example.com", None, &server.base_url(), "cid").unwrap();
    let drive = HttpDrive::new(session.clone(), server.url("/drivews"), server.url("/docws"), params());
    (drive, session)
}

fn file(name: &str, size: u64) -> Node {
    Node {
        drivewsid: format!("FILE::com.apple.CloudDocs::{name}"),
        docwsid: Some(name.to_owned()),
        etag: Some("etag-1".into()),
        zone: Some("com.apple.CloudDocs".into()),
        share_id: None,
        name: name.to_owned(),
        kind: NodeKind::File,
        size,
        modified: 0,
    }
}

fn folder(name: &str) -> Node {
    Node { kind: NodeKind::Folder, drivewsid: format!("FOLDER::com.apple.CloudDocs::{name}"), ..file(name, 0) }
}

#[test]
fn listing_a_folder_returns_only_its_direct_children() {
    let server = MockServer::start();
    let listing = server.mock(|when, then| {
        when.method(POST)
            .path("/drivews/retrieveItemDetailsInFolders")
            .query_param("dsid", "42")
            .query_param("clientId", "cid")
            .body_includes(r#""drivewsid":"FOLDER::com.apple.CloudDocs::Docs""#)
            .body_includes(r#""partialData":false"#);
        then.status(200).header("content-type", "application/json").json_body(json!([{
            "drivewsid": "FOLDER::com.apple.CloudDocs::Docs",
            "items": [
                {"drivewsid": "FILE::x::1", "docwsid": "1", "name": "a", "extension": "txt", "type": "FILE", "size": 5, "etag": "e"},
                {"drivewsid": "FOLDER::x::2", "name": "sub", "type": "FOLDER", "etag": "e"},
            ],
        }]));
    });
    let (drive, _) = drive(&server);
    let children = drive.children(&folder("Docs")).unwrap();
    listing.assert();
    assert_eq!(children.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(), ["a.txt", "sub"]);
}

#[test]
fn a_reply_without_items_is_a_protocol_error_not_an_empty_folder() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/drivews/retrieveItemDetailsInFolders");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(json!([{"drivewsid": "FOLDER::x::1", "status": "ERROR"}]));
    });
    let (drive, _) = drive(&server);
    let err = drive.children(&folder("x")).unwrap_err();
    assert!(matches!(err, Error::Protocol(msg) if msg.contains("ERROR")));
}

#[test]
fn downloads_resolve_the_content_url_and_stream_the_bytes() {
    let server = MockServer::start();
    let lookup = server.mock(|when, then| {
        when.method(GET).path("/docws/ws/com.apple.CloudDocs/download/by_id").query_param("document_id", "report.pdf");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(json!({"data_token": {"url": server.url("/content/blob")}}));
    });
    let blob = server.mock(|when, then| {
        when.method(GET).path("/content/blob").query_param("dsid", "42");
        then.status(200).body("hello world");
    });
    let (drive, _) = drive(&server);
    let mut out = String::new();
    drive.open(&file("report.pdf", 11)).unwrap().read_to_string(&mut out).unwrap();
    lookup.assert();
    blob.assert();
    assert_eq!(out, "hello world");
}

#[test]
fn zero_byte_files_are_not_requested_at_all() {
    let server = MockServer::start();
    let any = server.mock(|when, then| {
        when.any_request();
        then.status(400);
    });
    let (drive, _) = drive(&server);
    let mut out = Vec::new();
    drive.open(&file("empty", 0)).unwrap().read_to_end(&mut out).unwrap();
    assert!(out.is_empty());
    assert_eq!(any.calls(), 0);
}

#[test]
fn package_tokens_are_a_fallback_for_data_tokens() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/docws/ws/com.apple.CloudDocs/download/by_id");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(json!({"package_token": {"url": server.url("/content/pkg")}}));
    });
    server.mock(|when, then| {
        when.method(GET).path("/content/pkg");
        then.status(200).body("pkg");
    });
    let (drive, _) = drive(&server);
    let mut out = String::new();
    drive.open(&file("bundle", 3)).unwrap().read_to_string(&mut out).unwrap();
    assert_eq!(out, "pkg");
}

#[test]
fn a_failed_content_download_surfaces_as_an_error() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/docws/ws/com.apple.CloudDocs/download/by_id");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(json!({"data_token": {"url": server.url("/content/gone")}}));
    });
    server.mock(|when, then| {
        when.method(GET).path("/content/gone");
        then.status(404);
    });
    let (drive, _) = drive(&server);
    assert!(matches!(drive.open(&file("x", 3)), Err(Error::Status { status: 404, .. })));
}

#[test]
fn trash_and_permanent_deletes_use_different_endpoints() {
    let server = MockServer::start();
    let trash = server.mock(|when, then| {
        when.method(POST)
            .path("/drivews/moveItemsToTrash")
            .body_includes(r#""etag":"etag-1""#)
            .body_includes(r#""clientId":"FILE::com.apple.CloudDocs::a""#);
        then.status(200).header("content-type", "application/json").json_body(json!({}));
    });
    let forever = server.mock(|when, then| {
        when.method(POST).path("/drivews/deleteItems").body_includes(r#""clientId":"cid""#);
        then.status(200).header("content-type", "application/json").json_body(json!({}));
    });
    let (drive, _) = drive(&server);
    drive.delete(&file("a", 1), DeleteMode::Trash).unwrap();
    assert_eq!((trash.calls(), forever.calls()), (1, 0));
    drive.delete(&file("a", 1), DeleteMode::Permanent).unwrap();
    assert_eq!((trash.calls(), forever.calls()), (1, 1));
}

#[test]
fn folders_are_created_with_a_temporary_client_id_and_a_plain_text_body() {
    let server = MockServer::start();
    let create = server.mock(|when, then| {
        when.method(POST)
            .path("/drivews/createFolders")
            .header("content-type", "plain/text")
            .body_includes(r#""destinationDrivewsId":"FOLDER::com.apple.CloudDocs::Docs""#)
            .body_includes(r#""name":"New""#)
            .body_includes("FOLDER::UNKNOWN_ZONE::TempId-");
        then.status(200).header("content-type", "application/json").json_body(json!({}));
    });
    let (drive, _) = drive(&server);
    drive.create_folder(&folder("Docs"), "New").unwrap();
    create.assert();
}

#[test]
fn rename_and_move_carry_the_etag() {
    let server = MockServer::start();
    let rename = server.mock(|when, then| {
        when.method(POST).path("/drivews/renameItems").body_includes(r#""name":"b.txt""#).body_includes("etag-1");
        then.status(200).header("content-type", "application/json").json_body(json!({}));
    });
    let mv = server.mock(|when, then| {
        when.method(POST)
            .path("/drivews/moveItems")
            .body_includes(r#""destinationDrivewsId":"FOLDER::com.apple.CloudDocs::Dest""#)
            .body_includes("etag-1");
        then.status(200).header("content-type", "application/json").json_body(json!({}));
    });
    let (drive, _) = drive(&server);
    drive.rename(&file("a", 1), "b.txt").unwrap();
    drive.move_to(&file("a", 1), &folder("Dest")).unwrap();
    rename.assert();
    mv.assert();
}

#[test]
fn uploads_run_the_three_step_dance_and_send_the_file_bytes() {
    let server = MockServer::start();
    // The upload token travels in a cookie set by an earlier response.
    server.mock(|when, then| {
        when.method(GET).path("/prime");
        then.status(200).header("Set-Cookie", "X-APPLE-WEBAUTH-VALIDATE=v=1:t=UPLOAD-TOKEN:s=x; Path=/");
    });
    let slot = server.mock(|when, then| {
        when.method(POST)
            .path("/docws/ws/com.apple.CloudDocs/upload/web")
            .query_param("token", "UPLOAD-TOKEN")
            .header("content-type", "plain/text")
            .body_includes(r#""filename":"note.txt""#)
            .body_includes(r#""content_type":"text/plain""#)
            .body_includes(r#""size":11"#);
        then.status(200)
            .header("content-type", "application/json")
            .json_body(json!([{"document_id": "DOC-1", "url": server.url("/content/upload")}]));
    });
    let content = server.mock(|when, then| {
        when.method(POST)
            .path("/content/upload")
            .body_includes("hello world")
            .body_includes(r#"name="note.txt""#)
            .body_includes(r#"filename="note.txt""#);
        then.status(200).header("content-type", "application/json").json_body(json!({"singleFile": {
            "fileChecksum": "fc", "wrappingKey": "wk", "referenceChecksum": "rc", "size": 11, "receipt": "r1",
        }}));
    });
    let attach = server.mock(|when, then| {
        when.method(POST)
            .path("/docws/ws/com.apple.CloudDocs/update/documents")
            .header("content-type", "plain/text")
            .body_includes(r#""document_id":"DOC-1""#)
            .body_includes(r#""starting_document_id":"Docs""#)
            .body_includes(r#""path":"note.txt""#)
            .body_includes(r#""signature":"fc""#)
            .body_includes(r#""receipt":"r1""#)
            .body_includes(r#""mtime":1700000000000"#);
        then.status(200).header("content-type", "application/json").json_body(json!({}));
    });

    let (drive, session) = drive(&server);
    session.send(session.get(&server.url("/prime"))).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("note.txt");
    std::fs::File::create(&path).unwrap().write_all(b"hello world").unwrap();
    drive.upload(&folder("Docs"), "note.txt", std::fs::File::open(&path).unwrap(), 1_700_000_000).unwrap();

    slot.assert();
    content.assert();
    attach.assert();
}

#[test]
fn uploading_without_the_validate_cookie_asks_to_sign_in_again() {
    let server = MockServer::start();
    let (drive, _) = drive(&server);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f");
    std::fs::write(&path, b"x").unwrap();
    let err = drive.upload(&folder("Docs"), "f", std::fs::File::open(path).unwrap(), 0).unwrap_err();
    assert!(err.is_auth(), "{err:?}");
}

#[test]
fn api_errors_inside_a_200_reply_are_not_swallowed() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/drivews/renameItems");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(json!({"errorMessage": "name already in use", "errorCode": "CONFLICT"}));
    });
    let (drive, _) = drive(&server);
    let err = drive.rename(&file("a", 1), "b").unwrap_err();
    assert!(err.to_string().contains("name already in use"), "{err}");
}
